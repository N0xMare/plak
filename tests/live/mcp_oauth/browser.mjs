import { chromium } from "playwright";
import readline from "node:readline";
// The input and callback stay in pipes, never in evidence or Compose logs.
const reader = readline.createInterface({ input: process.stdin });
const input = await new Promise((resolve) => reader.once("line", resolve));
reader.close();
process.stdin.pause();
const { url, paste, code } = JSON.parse(input);
const browser = await chromium.launch({
  headless: true,
  args: ["--no-sandbox"],
});
try {
  const page = await browser.newPage();
  let callback;
  let deviceApproved = false;
  const callbackPattern = /^http:\/\/127\.0\.0\.1:\d+\/callback(?:\?|$)/;
  page.on("request", (request) => {
    if (callbackPattern.test(request.url())) callback = request.url();
  });
  await page.route(callbackPattern, async (route) => {
    callback = route.request().url();
    if (paste) await route.fulfill({ status: 200, body: "Callback captured" });
    else await route.continue();
  });
  await page.goto(url, { waitUntil: "domcontentloaded" }).catch(() => {});
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    if (code)
      deviceApproved =
        /Device Login Successful|Sign-in Success|You may close this browser window|successfully|approved/i.test(
          await page.locator("body").innerText(),
        );
    if (callback || deviceApproved) break;
    const userCode = page.locator(
      'input[name="user_code"]:visible, input[name="device_user_code"]:visible, input#user_code:visible',
    );
    if (code && (await userCode.count())) await userCode.first().fill(code);
    const user = page.locator('input[name="username"]');
    if (await user.count()) await user.first().fill("alice");
    const password = page.locator('input[type="password"]');
    if (await password.count()) await password.first().fill("fixture-only");
    const button = page
      .locator(
        'input[type="submit"], button[type="submit"], button[name="accept"], button:has-text("Yes")',
      )
      .first();
    if (await button.count())
      await button.click({ noWaitAfter: true, timeout: 3000 }).catch(() => {});
    await page.waitForTimeout(200);
  }
  if ((!code && !callback) || (code && !deviceApproved)) {
    const fields = await page
      .locator("input")
      .evaluateAll((inputs) => inputs.map((input) => input.name));
    throw new Error(
      "browser did not finish approval at " +
        new URL(page.url()).pathname.replace(
          /\/interaction\/[^/]+/,
          "/interaction/[id]",
        ) +
        "; input names: " +
        fields.join(","),
    );
  }
  const parsed = callback ? new URL(callback) : null;
  const authorize = new URL(url);
  const safe_callback = parsed
    ? {
        keys: [...parsed.searchParams.keys()],
        error: parsed.searchParams.get("error"),
        issuer: parsed.searchParams.get("iss"),
        state_matches:
          parsed.searchParams.get("state") ===
          authorize.searchParams.get("state"),
        redirect_matches:
          parsed.origin + parsed.pathname ===
          authorize.searchParams.get("redirect_uri"),
      }
    : null;
  process.stdout.write(
    JSON.stringify({ callback, device: !!code, safe_callback }),
  );
} finally {
  await browser.close();
}
