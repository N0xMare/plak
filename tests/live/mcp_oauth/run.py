#!/usr/bin/env python3
"""Opt-in, zero-inference MCP OAuth interoperability lane. No operator accounts."""
from __future__ import annotations

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
CONTROL = 'http://127.0.0.1:18082/control'
KC = 'http://127.0.0.1:18081'
RESOURCE = 'http://127.0.0.1:18082/mcp/'
SCENARIOS = (
    'dcr_loopback_keycloak', 'dcr_loopback_strict', 'dcr_paste', 'dcr_device',
    'confidential_issuer_pin', 'device_confidential', 'refresh_cadence',
    'rotation_reuse', 'forced_401', 'concurrent_refresh', 'step_up',
    'hostile_scope', 'server_revocation', 'logout_revoke', 'logout_503',
    'logout_local', 'resource_a_at_b', 'shared_origin_tenant',
    'issuer_mismatch', 'as_restart', 'state_key_carry',
)


def request(url, data=None, headers=None, method=None):
    if isinstance(data, dict):
        data = json.dumps(data).encode()
        headers = {'content-type': 'application/json', **(headers or {})}
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as response:
            return response.status, json.loads(response.read() or b'{}')
    except urllib.error.HTTPError as error:
        try:
            value = json.loads(error.read())
        except ValueError:
            value = {}
        return error.code, value


def authorize_url(output):
    # OSC 52 clipboard content is deliberately not decoded.
    for line in output.splitlines():
        value = line.strip()
        if value.startswith('http://127.0.0.1:') and 'code_challenge=' in value:
            parsed = urllib.parse.urlsplit(value)
            query = urllib.parse.parse_qs(parsed.query)
            if query.get('response_type') == ['code'] and query.get('code_challenge_method') == ['S256']:
                return value
    return None


def device_prompt(output):
    url = re.search(r'1\. Open:\s+(http://127\.0\.0\.1:\S+)', output)
    code = re.search(r'2\. Enter code:\s+([A-Za-z0-9-]+)', output)
    return (url.group(1), code.group(1)) if url and code else None


def redact(output):
    output = re.sub(r'\x1b\]52;[^\x07\x1b]*(?:\x07|\x1b\\)', '[clipboard omitted]', output)
    output = re.sub(r'https?://[^\s<>]+', lambda m: m.group(0).split('?')[0] + ('?[query omitted]' if '?' in m.group(0) else ''), output)
    output = re.sub(r'(Enter code:\s+)\S+', r'\1[omitted]', output)
    return output


def refresh_count(events, issuer='strict'):
    return sum(e.get('as') == issuer and e.get('grant') == 'refresh_token' for e in events)


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class Harness:
    def __init__(self, args):
        self.args = args
        self.binary = Path(args.binary).resolve()
        self.root = Path(tempfile.mkdtemp(prefix='plak-mcp-oauth-'))
        self.project = f'plak-oauth-{os.getpid()}'
        self.compose = ['docker', 'compose', '-p', self.project, '-f', str(HERE / 'compose.yaml')]
        self.output = []
        self.processes = set()
        self.shims = self.root / 'bin'
        self.shims.mkdir()
        for name in ('xdg-open', 'open'):
            path = self.shims / name
            path.write_text(f'#!{sys.executable}\nimport os, pathlib, sys\npathlib.Path(os.environ["OAUTH_BROWSER_URL"]).write_text(sys.argv[1])\n')
            path.chmod(0o700)
        for name in ('pbcopy', 'xclip', 'xsel', 'wl-copy'):
            path = self.shims / name
            path.write_text('#!/bin/sh\nexit 1\n')
            path.chmod(0o700)

    def compose_run(self, *args, **kwargs):
        return subprocess.run(self.compose + list(args), check=True, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, timeout=kwargs.pop('timeout', 180), **kwargs)

    def start(self):
        for port in (18080, 18081, 18082, 18083, 18084, 18765):
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', port))
        self.compose_run('run', '--rm', '-T', 'node', 'npm', 'ci', '--ignore-scripts', timeout=300)
        self.compose_run('up', '-d', 'keycloak', 'services')
        self.wait_url(f'{KC}/realms/mcp/.well-known/openid-configuration')
        self.wait_url('http://127.0.0.1:18083/.well-known/openid-configuration')
        self.admin_token = self.admin_login()
        base = f'{KC}/admin/realms/mcp'
        headers = {'authorization': f'Bearer {self.admin_token}'}
        status, policies = request(base + '/components?type=org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy', headers=headers)
        require(status == 200, 'Keycloak policy query failed')
        for policy in policies:
            if policy.get('subType') == 'anonymous':
                request(base + '/components/' + policy['id'], headers=headers, method='DELETE')
        realm_id = request(base, headers=headers)[1]['id']
        status, _ = request(base + '/components', {
            'name':'loopback-only-dcr','providerId':'trusted-hosts','parentId':realm_id,
            'providerType':'org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy',
            'subType':'anonymous', 'config':{'trusted-hosts':['127.0.0.1'],
            'host-sending-client-registration-request-must-match':['true'],
            'client-uris-must-match':['true']},
        }, headers=headers)
        require(status == 201, 'Keycloak anonymous DCR policy setup failed')
        # Every dynamically registered client is forced to S256 and consent.
        status, _ = request(base + '/components', {
            'name':'consent-required','providerId':'consent-required','parentId':realm_id,
            'providerType':'org.keycloak.services.clientregistration.policy.ClientRegistrationPolicy',
            'subType':'anonymous','config':{},
        }, headers=headers)
        require(status == 201, 'Keycloak consent policy setup failed')

    def wait_url(self, url):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                if request(url)[0] == 200:
                    return
            except (OSError, ValueError):
                pass
            time.sleep(0.5)
        raise RuntimeError('stack readiness deadline exceeded')

    def admin_login(self):
        status, result = request(f'{KC}/realms/master/protocol/openid-connect/token',
            urllib.parse.urlencode({'grant_type':'password','client_id':'admin-cli','username':'admin','password':'fixture-admin'}).encode())
        require(status == 200, 'Keycloak admin login failed')
        return result['access_token']

    def fixture(self, name, tenant='strict', confidential=False):
        self.output = []
        self.tenant = tenant
        self.home = self.root / name / 'home'
        (self.home / 'agents').mkdir(parents=True)
        self.home.chmod(0o700)
        self.workspace = self.root / name / 'workspace'
        self.workspace.mkdir()
        self.script = self.root / name / 'script.json'
        self.script.write_text(json.dumps({'version':1,'steps':[{'output':{'text':'done'}}]}))
        (self.home / 'agents' / 'lead.toml').write_text('account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\ncomposition = "direct"\ntools = ["mcp:srv"]\n')
        self.config = ('[root]\nagent = "lead"\n[orchestration]\nsubagents = false\n[compaction]\nenabled = false\n'
            f'[mcp_servers.srv]\ntransport = "http"\nurl = "{RESOURCE}{tenant}"\nauth = "oauth"\n')
        if confidential:
            self.config += (f'oauth_issuer = "{KC}/realms/mcp"\noauth_client_id = "confidential"\n'
                'oauth_client_secret = "fixture-client"\noauth_callback_port = 18765\n')
            (self.home / 'secrets').mkdir(mode=0o700)
            secret = self.home / 'secrets' / 'fixture-client.key'
            secret.write_text('fixture-client-secret')
            secret.chmod(0o600)
        self.write_config()
        for t in ('strict','keycloak','tenantb'):
            request(CONTROL, {'tenant':t,'force401':False,'hostile':False,'resource':None,'issuer':None})
        self.before = len(self.events())

    def write_config(self):
        (self.home / 'config.toml').write_text(self.config)

    def env(self, faults=''):
        env = {'PATH':str(self.shims) + os.pathsep + os.defpath,
            'HOME':str(self.root / 'fake-home'), 'TMPDIR':str(self.root),
            'XDG_STATE_HOME':str(self.root / 'xdg-state'), 'XDG_CONFIG_HOME':str(self.root / 'xdg-config'),
            'PLAK_HOME':str(self.home), 'PLAK_TEST_MODEL':'offline-loopback-v1',
            'PLAK_TEST_SCRIPT':str(self.script), 'PLAK_TEST_EVIDENCE':str(self.home.parent / 'model.jsonl'),
            'OAUTH_BROWSER_URL':str(self.home.parent / 'browser-url'),
            'BROWSER':str(self.shims / 'xdg-open'), 'PLAK_TEST_FAULTS':faults, 'TERM':'dumb'}
        return env

    def command(self, *args, faults='', binary=None):
        result = subprocess.run([str(binary or self.binary), *args], cwd=self.workspace,
            env=self.env(faults), text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=90)
        self.output.append({'args':list(args),'exit':result.returncode,'output':redact(result.stdout)})
        return result

    def browser(self, url, paste=False, code=None):
        result = self.compose_run('run', '--rm', '-T', 'browser', 'node', 'browser.mjs',
            input=json.dumps({'url':url,'paste':paste,'code':code}).encode(), timeout=90)
        return json.loads(result.stdout)

    def login(self, remote=False, expect_device=False, scope=None, tamper=False):
        args = [str(self.binary),'mcp','login','srv']
        if remote: args.append('--remote')
        if scope: args.extend(['--scope',scope])
        proc = subprocess.Popen(args, cwd=self.workspace, env=self.env(), stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        self.processes.add(proc)
        lines = queue.Queue()
        def read():
            for line in proc.stdout: lines.put(line)
        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        output = ''
        driven = False
        deadline = time.monotonic() + 110
        try:
            while time.monotonic() < deadline:
                try: output += lines.get(timeout=0.2)
                except queue.Empty: pass
                device = device_prompt(output)
                url = authorize_url(output)
                shim = self.home.parent / 'browser-url'
                if not remote and shim.exists(): url = shim.read_text()
                if not driven and device:
                    driven = True
                    self.browser(device[0], code=device[1])
                elif not driven and url and (remote or shim.exists()):
                    driven = True
                    if expect_device:
                        raise AssertionError('AS rejected DCR device grant; client fell back to paste')
                    callback = self.browser(url, paste=remote)['callback']
                    if remote:
                        if tamper:
                            p = urllib.parse.urlsplit(callback)
                            q = urllib.parse.parse_qs(p.query)
                            q['iss']=['https://wrong.example.invalid']
                            callback = urllib.parse.urlunsplit(p._replace(query=urllib.parse.urlencode(q,doseq=True)))
                        proc.stdin.write(callback + '\n'); proc.stdin.flush()
                if proc.poll() is not None:
                    thread.join(timeout=2)
                    while not lines.empty(): output += lines.get()
                    break
            else: raise RuntimeError('Plak login deadline exceeded')
            self.output.append({'args':args[1:],'exit':proc.returncode,'output':redact(output)})
            if tamper:
                require(proc.returncode != 0 and 'callback' in output, 'issuer mismatch not refused')
            else:
                require(proc.returncode == 0 and 'logged in' in output, 'login failed: ' + redact(output)[-1800:])
        finally:
            if proc.poll() is None: proc.terminate()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()
            self.processes.discard(proc)
            shim = self.home.parent / 'browser-url'
            shim.unlink(missing_ok=True)

    def events(self):
        return request(CONTROL)[1]['events']

    def delta(self):
        return self.events()[self.before:]

    def control(self, **values):
        return request(CONTROL, {'tenant':self.tenant, **values})

    def test(self, ok=True, faults=''):
        result = self.command('mcp','test','srv',faults=faults)
        require((result.returncode == 0) == ok, 'unexpected MCP test result: ' + redact(result.stdout)[-1000:])
        return result.stdout

    def tool(self, name='read', faults='', session=None):
        self.script.write_text(json.dumps({'version':1,'steps':[
            {'output':{'tool':{'name':'mcp__srv__'+name,'arguments':{}}},'finish':'tool_calls'},
            {'output':{'text':'done'}}]}))
        args=['run','--json']
        if session: args += ['--session',session]
        result=self.command(*args,'exercise MCP fixture',faults=faults)
        # A model's final text never proves that the MCP call ran.
        records=[]
        for line in result.stdout.splitlines():
            try: records.append(json.loads(line))
            except ValueError: pass
        sid=next((r.get('session_id') or r.get('session') for r in records if r.get('session_id') or isinstance(r.get('session'),str)),None)
        if not sid:
            match=re.search(r'"(?:session_id|id)"\s*:\s*"([0-9a-f-]{36})"',result.stdout)
            sid=match.group(1) if match else None
        if sid:
            log=self.command('session','log',sid,'--json')
            require(log.returncode == 0,'session log unavailable')
        return result, sid

    def scenario(self, name):
        tenant='keycloak' if name in ('dcr_loopback_keycloak','confidential_issuer_pin','device_confidential','server_revocation','as_restart') else 'strict'
        self.fixture(name,tenant,confidential=name in ('confidential_issuer_pin','device_confidential'))
        if name == 'state_key_carry':
            if not self.args.previous_binary:
                return 'BLOCKED', 'requires --previous-binary with a different state key and MCP OAuth support'
            previous=Path(self.args.previous_binary).resolve()
            oldkey=self.command('version','--state-key',binary=previous).stdout.strip()
            newkey=self.command('version','--state-key').stdout.strip()
            require(oldkey != newkey,'carry binaries must have different state keys')
            self.login()
            current=self.binary
            self.binary=previous
            try: result,sid=self.tool()
            finally: self.binary=current
            require(result.returncode == 0 and sid,'old-build session creation failed')
            carry=self.home.parent / 'carry.json'
            require(self.command('session','export',sid,'--out',str(carry),binary=previous).returncode==0,'export failed')
            imported=self.command('session','import',str(carry))
            require(imported.returncode==0,'import failed')
            self.test()
            return 'PASS','state-key move retained home-scoped login'
        if name == 'dcr_device':
            self.login(remote=True,expect_device=True)
        elif name == 'dcr_paste':
            self.login(remote=True)
        elif name == 'device_confidential':
            self.login(remote=True,expect_device=True)
        elif name == 'issuer_mismatch':
            self.login(remote=True,tamper=True)
            require(not any(e.get('issued') for e in self.delta()),'token issued after issuer mismatch')
            return 'PASS','callback refused before token dispatch'
        else:
            self.login()
        if name.startswith('dcr_') or name in ('confidential_issuer_pin','device_confidential'):
            self.test()
            if name != 'dcr_device':
                result,_=self.tool()
                require(result.returncode==0,'scripted tool turn failed')
                require(any(e.get('event')=='effect' for e in self.delta()),'no MCP effect observed')
        elif name in ('refresh_cadence','rotation_reuse','concurrent_refresh'):
            self.test(); self.test()
            require(refresh_count(self.delta())==0,'60s token refreshed before half its lifetime')
            # This is the behavior under test, not a readiness poll.
            time.sleep(32)
            if name=='concurrent_refresh':
                with concurrent.futures.ThreadPoolExecutor(2) as pool:
                    results=list(pool.map(lambda _: self.command('mcp','test','srv'), range(2)))
                require(refresh_count(self.delta())==1,'concurrent processes did not collapse refresh')
                require(all(r.returncode==0 for r in results),'refresh contender refused instead of sharing renewed login')
            else:
                self.test();self.test()
                require(refresh_count(self.delta())==1,'refresh cadence was not once per half-lifetime')
                if name=='rotation_reuse':
                    status,reuse=self.control(reuse='strict')
                    require(status==200 and reuse.get('error')=='invalid_grant','AS did not detect predecessor refresh-token reuse')
                    time.sleep(32)
                    self.test(ok=False)
        elif name=='forced_401':
            self.control(force401=True)
            result,_=self.tool()
            events=self.delta()
            require(refresh_count(events)==1,'401 did not cause exactly one refresh')
            require(sum(e.get('method')=='tools/call' for e in events)==1,'401 call was resent')
            require(not any(e.get('event')=='effect' for e in events),'rejected tool ran')
            require('call again' in result.stdout or 'renewed' in result.stdout,'missing renewed-login remedy')
            self.tool()
            require(sum(e.get('event')=='effect' for e in self.delta())==1,'next explicit call did not succeed exactly once')
        elif name in ('step_up','hostile_scope'):
            if name=='hostile_scope': self.control(hostile=True)
            result,_=self.tool('write')
            require(any(e.get('status')==403 for e in self.delta()),'no 403 challenge observed')
            require(not any(e.get('event')=='effect' for e in self.delta()),'under-scoped call ran')
            require('plak mcp login srv' in result.stdout,'no login remedy')
            require('$(touch' not in result.stdout,'hostile scope reached remedy')
            if name=='step_up':
                self.login(scope='read write')
                self.tool('write')
                require(any(e.get('event')=='effect' and e.get('tool')=='write' for e in self.delta()),'step-up did not authorize tool')
        elif name=='server_revocation':
            self.admin_token=self.admin_login()
            status,_=request(f'{KC}/admin/realms/mcp/logout-all',headers={'authorization':f'Bearer {self.admin_token}'},method='POST')
            require(status==204,'Keycloak server-side revocation failed')
            # Keycloak JWTs remain valid until expiry; refresh then witnesses invalid_grant.
            time.sleep(32)
            output=self.test(ok=False)
            require('invalid_grant' in output and output.count('remedy: plak mcp login srv')==1,'missing single invalid_grant remedy')
            mark=len(self.events());self.test(ok=False)
            require(not self.events()[mark:],'retired login sent another request')
        elif name in ('logout_revoke','logout_503','logout_local'):
            faults='host=127.0.0.1,path=/token/revocation,nth=1,fault=503' if name=='logout_503' else ''
            args=['mcp','logout','srv']+(['--local'] if name=='logout_local' else [])
            result=self.command(*args,faults=faults)
            if name=='logout_503':
                require(result.returncode!=0,'503 logout succeeded')
                shown=self.command('mcp','show','srv').stdout
                require('quarantined' in shown,'failed revocation did not quarantine')
                self.test(ok=False)
                require(self.command('mcp','logout','srv','--local').returncode==0,'local retirement failed')
            else:
                require(result.returncode==0,'logout failed')
                revokes=[e for e in self.delta() if e.get('revoked')]
                require(bool(revokes)==(name=='logout_revoke'),'unexpected revocation dispatch count')
            require('not logged in' in self.command('mcp','show','srv').stdout,'login not retired')
        elif name=='resource_a_at_b':
            # In-memory fixture handoff, never print or persist the bearer outside its private store.
            self.test()
            status,value=self.control(probe_audience=True)
            require(status==200 and value.get('status')==401,'resource A token was not refused at B')
        elif name=='shared_origin_tenant':
            expectations=json.loads(Path(self.args.resource_rules).read_text())
            self.control(resource=RESOURCE+'tenantb')
            before=len(self.events())
            try: self.login()
            except AssertionError:
                require(expectations['tenant_metadata']=='refuse','resource rules expected tenant login')
                require(not any(e.get('issued') for e in self.events()[before:]),'tenant attack reached token issuance')
            else: require(expectations['tenant_metadata']=='accept','shared-origin tenant metadata accepted')
        elif name=='as_restart':
            self.test()
            self.compose_run('restart','keycloak')
            self.wait_url(f'{KC}/realms/mcp/.well-known/openid-configuration')
            self.test()
            time.sleep(32)
            self.test()
        return 'PASS','external AS and MCP witnesses verified'

    def close(self):
        for proc in list(self.processes):
            if proc.poll() is None: proc.kill();proc.wait()
        try: self.compose_run('down','--volumes','--remove-orphans',timeout=90)
        finally: shutil.rmtree(self.root)


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',default=str(ROOT/'target/release/plak'))
    parser.add_argument('--previous-binary')
    parser.add_argument('--only',help='comma-separated scenario names')
    parser.add_argument('--report',default=str(ROOT/'target/live/mcp-oauth/report.json'))
    parser.add_argument('--resource-rules',default=str(HERE/'resource-rules.json'))
    parser.add_argument('--list',action='store_true')
    args=parser.parse_args(argv)
    if args.list:
        print('\n'.join(SCENARIOS));return 0
    selected=args.only.split(',') if args.only else list(SCENARIOS)
    if set(selected)-set(SCENARIOS): parser.error('unknown scenario')
    # Fixed loopback ports, guarded across checkouts; never interfere with another lane.
    lock=open(Path(tempfile.gettempdir())/'plak-mcp-oauth.lock','a')
    fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    os.umask(0o077)
    harness=Harness(args)
    digest=hashlib.sha256(harness.binary.read_bytes()).hexdigest()
    report={'binary_sha256':digest,'source_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
            'scenarios':[],'paid_inference':False}
    try:
        version=subprocess.check_output([str(harness.binary),'version'],text=True)
        require('test-hooks' in version,'binary must be built with test-hooks')
        report['version']=version.strip()
        harness.start()
        for name in selected:
            start=time.monotonic()
            try: result,detail=harness.scenario(name)
            except AssertionError as error: result,detail='FAIL',str(error)
            except Exception as error: result,detail='HARNESS',type(error).__name__+': '+redact(str(error))
            row={'name':name,'result':result,'detail':detail,'seconds':round(time.monotonic()-start,2),
                 'commands':list(harness.output),'events':harness.delta()}
            report['scenarios'].append(row)
            print(f'{name}: {result}: {detail}',flush=True)
    except Exception as error:
        report['setup_error']=type(error).__name__+': '+redact(str(error))
        print(report['setup_error'],file=sys.stderr)
    finally:
        try: harness.close();report['teardown']='complete'
        except Exception as error: report['teardown']='failed: '+type(error).__name__
        report['binary_unchanged']=hashlib.sha256(harness.binary.read_bytes()).hexdigest()==digest
        path=Path(args.report);path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(json.dumps(report,indent=2)+'\n')
        print(f'report: {path}')
    return 0 if len(report['scenarios'])==len(selected) and all(s['result']=='PASS' for s in report['scenarios']) and report.get('teardown')=='complete' and report['binary_unchanged'] else 1


if __name__=='__main__':
    raise SystemExit(main())
