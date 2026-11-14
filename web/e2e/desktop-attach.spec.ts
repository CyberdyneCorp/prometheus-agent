// The desktop attached through its bundled binary (spec: desktop-app,
// Daemon attachment through the bundled binary; Sessions are created with
// desktop source; One-use dial tickets never reach the webview's storage;
// Windows never open themselves; External URL scheme allowlist; Deep
// links).
import { expect, test } from "@playwright/test";
import { emit, issuedTickets, openDesktop, parseLink, say, sent } from "./desktop";
import { cli } from "./helpers";

test("a new chat is created with desktop source", async ({ page }) => {
  const sockets = await openDesktop(page);
  const hello = JSON.parse(sockets[0][0]) as { method: string; params: Record<string, unknown> };
  expect(hello.method).toBe("rpc.hello");
  expect(hello.params).toMatchObject({ client: "desktop", capabilities: { server_requests: true } });
  const id = await say(page, "hello");
  expect(sent(sockets, "session.create")[0].params).toMatchObject({ source: "desktop", profile: "default" });
  const shown = JSON.parse(cli("--json", "sessions", "show", id)) as { session: { source: string } };
  expect(shown.session.source).toBe("desktop");
});

test("dial tickets never reach the webview's storage and a replayed one is refused", async ({ page }) => {
  await openDesktop(page);
  await page.evaluate(() => localStorage.setItem("probe", "1"));
  const tickets = await issuedTickets();
  expect(tickets.length).toBeGreaterThan(0);
  const stored = await page.evaluate(async () => {
    const dbs = (await indexedDB.databases()).map((d) => d.name ?? "");
    const cached = "caches" in window ? await caches.keys() : [];
    return [
      location.href,
      document.cookie,
      document.documentElement.outerHTML,
      JSON.stringify({ ...localStorage }),
      JSON.stringify({ ...sessionStorage }),
      JSON.stringify(dbs),
      JSON.stringify(cached),
    ].join("\n");
  });
  expect(stored).toContain("probe");
  for (const ticket of tickets) expect(stored).not.toContain(ticket);
  // The daemon closes a socket that presents a consumed ticket with 4401.
  const status = JSON.parse(cli("--json", "daemon", "status")) as { daemon: { rpc_url: string } };
  const code = await page.evaluate(
    ([url, ticket]) =>
      new Promise<number>((resolve) => {
        const ws = new WebSocket(url, ["prometheus-rpc.v1", `prometheus-ticket.${ticket}`]);
        ws.onclose = (event) => resolve(event.code);
      }),
    [status.daemon.rpc_url, tickets[0]],
  );
  expect(code).toBe(4401);
});

test("page content opens no window and a javascript link runs nothing", async ({ page, context }) => {
  await openDesktop(page);
  let popups = 0;
  context.on("page", () => popups++);
  let dialogs = 0;
  page.on("dialog", (dialog) => {
    dialogs++;
    void dialog.dismiss();
  });
  expect(await page.evaluate(() => window.open("https://attacker.example") === null)).toBe(true);
  await page.evaluate(() => {
    const link = document.createElement("a");
    link.href = "javascript:alert(1)";
    link.id = "agent-link";
    link.textContent = "click me";
    document.querySelector(".transcript")?.append(link);
  });
  await page.click("#agent-link");
  await expect(page.locator(".notices")).toContainText("Not opened (invalid): javascript:alert(1)");
  const answer = await page.evaluate(() =>
    (window as unknown as { __TAURI__: { core: { invoke: (c: string, a: unknown) => Promise<unknown> } } }).__TAURI__.core.invoke("shell", {
      request: { command: "open_external", args: { url: "javascript:alert(1)" } },
    }),
  );
  expect(answer).toEqual({ ok: false, reason: "invalid" });
  expect(dialogs).toBe(0);
  expect(popups).toBe(0);
});

test("a deep link with a prompt payload opens an empty draft and submits nothing", async ({ page }) => {
  const sockets = await openDesktop(page);
  await say(page, "first");
  const submitted = sent(sockets, "prompt.submit").length;
  const link = await parseLink("prometheus://new?profile=default&prompt=rm%20-rf");
  expect(link).toEqual({ open: "new", profile: "default" });
  await emit(page, "deep-link", link);
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", "");
  await expect(page.locator("textarea[name=prompt]")).toHaveValue("");
  await expect(page.locator(".chat-title")).toHaveText("New chat");
  expect(sent(sockets, "prompt.submit").length).toBe(submitted);
  expect(await parseLink("prometheus://settings/reset")).toBeNull();
});
