// Scenario: OIDC viewer submits (web-dashboard, Remote dashboard users are
// observers by default). A user signed in through OIDC who is not in
// dashboard.auth.drivers sees a disabled composer, and a forced
// prompt.submit over /api/rpc is refused with -32002.
import { expect, test } from "@playwright/test";
import { csrf, env, signIn } from "./helpers";

test("an OIDC viewer cannot send", async ({ browser }) => {
  const owner = await browser.newPage();
  const url = await signIn(owner);
  await owner.goto(`${url}chat`);
  await owner.getByLabel("Message").fill("hello");
  await owner.getByRole("button", { name: "Send" }).click();
  await expect(owner.locator(".message.assistant")).toHaveCount(1);
  const session = new URL(owner.url()).searchParams.get("session") ?? "";

  const viewer = await (await browser.newContext()).newPage();
  await viewer.goto(`${url}login`);
  await expect(viewer.locator("header.top")).toBeVisible();
  await expect(viewer.locator(".role")).toHaveText("observer");
  await viewer.goto(`${url}chat?session=${session}`);
  await expect(viewer.locator(".message.assistant")).toHaveCount(1);
  await expect(viewer.getByLabel("Message")).toBeDisabled();
  await expect(viewer.getByRole("button", { name: "Send" })).toBeDisabled();
  await viewer.goto(`${url}pairing`);
  await expect(viewer.getByText("You are an observer: approving needs a driver.")).toBeVisible();
  await viewer.goto(`${url}chat?session=${session}`);

  const token = await csrf(viewer, url);
  const code = await viewer.evaluate(
    async ({ session, token }) => {
      const ws = new WebSocket(`ws://${location.host}/api/rpc`, ["prometheus-rpc.v1", `csrf.${token}`]);
      const replies = new Map<number, { error?: { code: number } }>();
      ws.onmessage = (m) => {
        const frame = JSON.parse(String(m.data));
        if (typeof frame.id === "number") replies.set(frame.id, frame);
      };
      await new Promise((r) => (ws.onopen = r));
      const call = async (id: number, method: string, params: unknown) => {
        ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
        while (!replies.has(id)) await new Promise((r) => setTimeout(r, 20));
        return replies.get(id);
      };
      await call(1, "rpc.hello", { client: "dashboard", client_version: "t", protocol: { major: 1, minor: 0 }, capabilities: { server_requests: true, events: [] } });
      await call(2, "session.attach", { session_id: session, role: "driver" });
      return (await call(3, "prompt.submit", { session_id: session, text: "forced" }))?.error?.code;
    },
    { session, token },
  );
  expect(code).toBe(-32002);
  expect(env("PMT_HOME")).toBeTruthy();
});
