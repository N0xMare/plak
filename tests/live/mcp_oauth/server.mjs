import http from "node:http";
import { randomUUID } from "node:crypto";
import Provider from "oidc-provider";
import {
  createRemoteJWKSet,
  jwtVerify,
  generateKeyPair,
  exportJWK,
} from "jose";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";

const origin = "http://127.0.0.1:18082";
const strictIssuer = "http://127.0.0.1:18083";
const keycloakIssuer = "http://127.0.0.1:18081/realms/mcp";
const events = [];
const controls = {};
const refreshes = new Map();
let lastStrictAccess;
const tenants = {
  keycloak: {
    issuer: keycloakIssuer,
    jwks: `${keycloakIssuer}/protocol/openid-connect/certs`,
  },
  strict: { issuer: strictIssuer, jwks: `${strictIssuer}/jwks` },
  tenantb: { issuer: strictIssuer, jwks: `${strictIssuer}/jwks` },
};
function record(e) {
  events.push({ at: Date.now(), ...e });
}
async function body(req) {
  let data = "";
  for await (const c of req) data += c;
  return data;
}
function json(res, status, value) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(value));
}
const { privateKey } = await generateKeyPair("RS256", { extractable: true });
const signingKey = await exportJWK(privateKey);
signingKey.kid = randomUUID();
signingKey.use = "sig";
signingKey.alg = "RS256";
const oidc = new Provider(strictIssuer, {
  jwks: { keys: [signingKey] },
  clients: [],
  cookies: { keys: [randomUUID(), randomUUID()] },
  scopes: ["openid", "offline_access", "read", "write"],
  pkce: { required: () => true, methods: ["S256"] },
  features: {
    devInteractions: { enabled: false },
    registration: { enabled: true },
    revocation: { enabled: true },
    deviceFlow: { enabled: true },
    resourceIndicators: {
      enabled: true,
      defaultResource: () => `${origin}/mcp/strict`,
      getResourceServerInfo: (_ctx, resource) => {
        if (
          ![`${origin}/mcp/strict`, `${origin}/mcp/tenantb`].includes(resource)
        )
          throw new Error("invalid resource");
        return {
          scope: "read write",
          audience: resource,
          accessTokenFormat: "jwt",
          accessTokenTTL: 60,
        };
      },
      useGrantedResource: () => true,
    },
  },
  ttl: { AccessToken: 60, RefreshToken: 600 },
  rotateRefreshToken: () => true,
  issueRefreshToken: () => true,
  findAccount: async (_ctx, id) => ({
    accountId: id,
    claims: async () => ({ sub: id }),
  }),
  interactions: {
    url: (_ctx, interaction) => `/interaction/${interaction.uid}`,
  },
});
oidc.on("grant.success", () => record({ as: "strict", event: "grant" }));
oidc.on("grant.revoked", () =>
  record({ as: "strict", event: "grant_revoked" }),
);
oidc.on("refresh_token.consumed", () =>
  record({ as: "strict", event: "rotation" }),
);
oidc.proxy = true;
const oidcCallback = oidc.callback();
http
  .createServer(async (req, res) => {
    try {
      if (req.url.startsWith("/interaction/")) {
        const details = await oidc.interactionDetails(req, res);
        if (req.method === "GET") {
          res.setHeader("content-type", "text/html");
          res.end(
            '<form method="post"><input name="username"><input name="password" type="password"><button type="submit">Approve</button></form>',
          );
          return;
        }
        const form = new URLSearchParams(await body(req));
        if (
          form.get("username") !== "alice" ||
          form.get("password") !== "fixture-only"
        )
          return json(res, 403, {});
        if (details.prompt.name === "login") {
          await oidc.interactionFinished(
            req,
            res,
            { login: { accountId: "alice" } },
            { mergeWithLastSubmission: false },
          );
        } else {
          let grant = details.grantId
            ? await oidc.Grant.find(details.grantId)
            : new oidc.Grant({
                accountId: "alice",
                clientId: details.params.client_id,
              });
          const d = details.prompt.details;
          if (d.missingOIDCScope)
            grant.addOIDCScope(d.missingOIDCScope.join(" "));
          for (const [resource, scopes] of Object.entries(
            d.missingResourceScopes || {},
          ))
            grant.addResourceScope(resource, scopes.join(" "));
          await oidc.interactionFinished(
            req,
            res,
            { consent: { grantId: await grant.save() } },
            { mergeWithLastSubmission: true },
          );
        }
        return;
      }
      oidcCallback(req, res);
    } catch {
      json(res, 500, { error: "fixture_interaction_failure" });
    }
  })
  .listen(18084, "127.0.0.1");

// AS-facing proxies observe actual dispatches without retaining authorization codes,
// credentials, JWTs or refresh token bytes in the evidence stream.
for (const [port, upstream, label] of [
  [18081, 18080, "keycloak"],
  [18083, 18084, "strict"],
  [18085, 18080, "keycloak"],
]) {
  http
    .createServer(async (req, res) => {
      try {
        const data = await body(req);
        const params = new URLSearchParams(data);
        const query = new URL(req.url, `http://127.0.0.1:${port}`).searchParams;
        if (query.get("response_type") === "code")
          record({
            as: label,
            event: "authorize",
            resource: query.get("resource"),
            pkce: query.get("code_challenge_method"),
          });
        if (
          params.get("grant_type") === "refresh_token" &&
          controls[label]?.refreshDelayMs
        )
          await new Promise((resolve) =>
            setTimeout(resolve, controls[label].refreshDelayMs),
          );
        const headers = {
          ...req.headers,
          host: `127.0.0.1:${label === "keycloak" ? 18081 : port}`,
          "x-forwarded-host": `127.0.0.1:${label === "keycloak" ? 18081 : port}`,
          "x-forwarded-proto": "http",
        };
        delete headers["content-length"];
        const response = await fetch(`http://127.0.0.1:${upstream}${req.url}`, {
          method: req.method,
          headers,
          body: ["GET", "HEAD"].includes(req.method) ? undefined : data,
          redirect: "manual",
        });
        let bytes = Buffer.from(await response.arrayBuffer());
        if (controls[label]?.hideDevice && req.url.includes(".well-known/") && response.status === 200) {
          const metadata = JSON.parse(bytes);
          delete metadata.device_authorization_endpoint;
          bytes = Buffer.from(JSON.stringify(metadata));
        }
        if (label === "keycloak" && controls.keycloak?.splitEndpoints &&
            req.url.includes(".well-known/") && response.status === 200) {
          const metadata = JSON.parse(bytes);
          for (const key of ["token_endpoint", "device_authorization_endpoint", "revocation_endpoint"])
            if (metadata[key]) metadata[key] = metadata[key].replace(":18081/", ":18085/");
          bytes = Buffer.from(JSON.stringify(metadata));
        }
        if (
          /token$|revoke$|revocation$|\/reg$|\/registrations|\/device\/auth$|\/auth\/device$/.test(
            req.url,
          )
        ) {
          let result = {};
          try {
            result = JSON.parse(bytes);
          } catch {}
          record({
            as: label,
            event: "http",
            path: req.url.split("?")[0],
            destination_port: port,
            client_secret_matches: params.get("client_secret") === "fixture-client-secret" ||
              req.headers.authorization === "Basic " + Buffer.from("confidential:fixture-client-secret").toString("base64") ||
              req.headers.authorization === "Basic " + Buffer.from("device-confidential:fixture-client-secret").toString("base64"),
            grant: params.get("grant_type"),
            status: response.status,
            error: result.error,
            resource: params.get("resource"),
            issued: !!result.access_token,
            client_grants: result.grant_types,
            revoked: /revoke$|revocation$/.test(req.url),
          });
          if (label === "strict" && result.access_token)
            lastStrictAccess = result.access_token;
          if (result.refresh_token) {
            const prior = refreshes.get(label);
            refreshes.set(label, {
              current: result.refresh_token,
              previous: prior?.current,
              client: params.get("client_id"),
              secret: params.get("client_secret"),
              authorization: req.headers.authorization,
              endpoint: req.url,
              resource: params.get("resource"),
            });
          }
        }
        const out = Object.fromEntries(response.headers);
        delete out["content-encoding"];
        delete out["content-length"];
        delete out["transfer-encoding"];
        // Preserve multiple session cookies across the proxy.
        if (response.headers.getSetCookie)
          out["set-cookie"] = response.headers.getSetCookie();
        res.writeHead(response.status, out);
        res.end(bytes);
      } catch {
        json(res, 502, { error: "fixture_proxy_failure" });
      }
    })
    .listen(port, "127.0.0.1");
}

const jwks = Object.fromEntries(
  Object.entries(tenants).map(([name, t]) => [
    name,
    createRemoteJWKSet(new URL(t.jwks)),
  ]),
);
http
  .createServer(async (req, res) => {
    try {
      const url = new URL(req.url, origin);
      if (url.pathname === "/control") {
        if (req.method === "GET") return json(res, 200, { events });
        const c = JSON.parse(await body(req));
        if (c.probe_audience) {
          const response = await fetch(`${origin}/mcp/tenantb`, {
            method: "POST",
            headers: {
              authorization: `Bearer ${lastStrictAccess}`,
              "content-type": "application/json",
            },
            body: JSON.stringify({
              jsonrpc: "2.0",
              id: 1,
              method: "tools/list",
            }),
          });
          return json(res, 200, { status: response.status });
        }
        if (c.reuse) {
          const r = refreshes.get(c.reuse);
          if (!r?.previous) return json(res, 409, { error: "no_predecessor" });
          const data = new URLSearchParams({
            grant_type: "refresh_token",
            refresh_token: r.previous,
            client_id: r.client || "",
            resource: r.resource || "",
          });
          if (r.secret) data.set("client_secret", r.secret);
          const response = await fetch(
            `http://127.0.0.1:${c.reuse === "strict" ? 18083 : 18081}${r.endpoint}`,
            {
              method: "POST",
              body: data,
              headers: r.authorization
                ? { authorization: r.authorization }
                : {},
            },
          );
          const result = await response.json();
          return json(res, 200, {
            status: response.status,
            error: result.error,
          });
        }
        Object.assign((controls[c.tenant || "strict"] ||= {}), c);
        return json(res, 200, { ok: true });
      }
      const metadata = url.pathname.startsWith(
        "/.well-known/oauth-protected-resource",
      );
      const name = url.pathname.split("/").at(-1);
      const tenant = tenants[name];
      if (!tenant) return json(res, 404, {});
      const resource = `${origin}/mcp/${name}`;
      const mode = controls[name] || {};
      if (metadata)
        return json(res, 200, {
          resource: mode.resource || resource,
          authorization_servers: [mode.issuer || tenant.issuer],
          scopes_supported: ["read"],
        });
      const challenge = (status, extra = "") => {
        res.setHeader(
          "www-authenticate",
          `Bearer resource_metadata="${origin}/.well-known/oauth-protected-resource/mcp/${name}"${extra}`,
        );
        json(res, status, {
          error: status === 403 ? "insufficient_scope" : "invalid_token",
        });
      };
      let message;
      const raw = await body(req);
      try {
        message = JSON.parse(raw);
      } catch {}
      let claims;
      const bearer = req.headers.authorization?.replace(/^Bearer /, "");
      if (!bearer) return challenge(401);
      try {
        claims = (
          await jwtVerify(bearer, jwks[name], {
            issuer: tenant.issuer,
            audience: resource,
          })
        ).payload;
      } catch {
        record({
          event: "mcp",
          tenant: name,
          method: message?.method,
          status: 401,
          audience_ok: false,
        });
        return challenge(401);
      }
      if (mode.force401 && message?.method === "tools/call") {
        mode.force401 = false;
        record({
          event: "mcp",
          tenant: name,
          method: message.method,
          status: 401,
          audience_ok: true,
        });
        return challenge(401);
      }
      if (
        message?.method === "tools/call" &&
        (mode.hostile ||
          (message.params?.name === "write" &&
            !claims.scope?.split(" ").includes("write")))
      ) {
        const scope = mode.hostile
          ? "$(touch /tmp/plak-oauth-hostile)"
          : "write";
        record({
          event: "mcp",
          tenant: name,
          method: message.method,
          status: 403,
          audience_ok: true,
        });
        return challenge(403, `, error="insufficient_scope", scope="${scope}"`);
      }
      res.once("finish", () =>
        record({
          event: "mcp",
          tenant: name,
          method: message?.method,
          status: res.statusCode,
          audience_ok: true,
          audience: claims.aud,
        }),
      );
      if (req.method !== "POST") {
        res.writeHead(405);
        res.end();
        return;
      }
      const server = new McpServer({ name: "oauth-e2e", version: "1.0.0" });
      for (const tool of ["read", "write"])
        server.tool(tool, {}, async () => {
          record({ event: "effect", tenant: name, tool });
          return { content: [{ type: "text", text: `${name}:${tool}:ok` }] };
        });
      const transport = new StreamableHTTPServerTransport({
        sessionIdGenerator: undefined,
        enableJsonResponse: true,
      });
      await server.connect(transport);
      res.on("close", () => {
        transport.close();
        server.close();
      });
      await transport.handleRequest(req, res, message);
    } catch {
      if (!res.headersSent) json(res, 500, { error: "fixture_server_failure" });
      else res.end();
    }
  })
  .listen(18082, "127.0.0.1");
