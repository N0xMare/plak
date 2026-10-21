import { chromium } from 'playwright';
// The input and callback stay in pipes, never in evidence or Compose logs.
let input = '';
for await (const chunk of process.stdin) input += chunk;
const { url, paste, code } = JSON.parse(input);
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] });
try {
  const page = await browser.newPage();
  let callback;
  await page.route('**/callback?**', async route => {
    callback = route.request().url();
    if (paste) await route.fulfill({status:200, body:'Callback captured'});
    else await route.continue();
  });
  await page.goto(url, {waitUntil:'domcontentloaded'}).catch(() => {});
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    if (callback || (code && /approved|success|complete/i.test(await page.locator('body').innerText()))) break;
    const userCode = page.locator('input[name="user_code"], input#user_code');
    if (code && await userCode.count()) await userCode.first().fill(code);
    const user = page.locator('input[name="username"]');
    if (await user.count()) await user.first().fill('alice');
    const password = page.locator('input[type="password"]');
    if (await password.count()) await password.first().fill('fixture-only');
    const button = page.locator('input[type="submit"], button[type="submit"], button[name="accept"], button:has-text("Yes")').first();
    if (await button.count()) await button.click().catch(() => {});
    await page.waitForTimeout(200);
  }
  if (!code && !callback) throw new Error('browser did not reach callback');
  process.stdout.write(JSON.stringify({ callback, device: !!code }));
} finally { await browser.close(); }
