// The realtime voice chat mode (spec: voice-mode, Realtime full-duplex
// voice chat mode (upgrade)), driven by crates/prometheus/tests/
// desktop_voice.rs: the daemon creates the call at a stand-in provider
// with the profile's key; the page's WebRTC peer is a stand-in too (the
// provider's model is played by this spec on the data channel), so the
// page's half is exercised as it runs: its offer, the answer it applies,
// the agent tool it runs as a turn of the chat and the result it hands
// back, and the usage it reports.
import { expect, test } from "@playwright/test";
import { openDesktop, sent } from "./desktop";
import { env } from "./helpers";
import { fakeMicrophone, voiceBrowser, received } from "./voice";

test.use(voiceBrowser());

interface Realtime {
  answer?: string;
  closed?: boolean;
  sent: { type: string; item?: { type?: string; call_id?: string; output?: string } }[];
}

test("the realtime voice hands the request to the agent and never sees the key", async ({ page }) => {
  await fakeMicrophone(page);
  await page.addInitScript(() => {
    const rt = { sent: [] as unknown[] } as Record<string, unknown> & { sent: unknown[] };
    (window as unknown as { __rt: unknown }).__rt = rt;
    class Channel {
      readyState = "open";
      onmessage: ((event: { data: string }) => void) | null = null;
      send(data: string): void {
        rt.sent.push(JSON.parse(data) as unknown);
      }
    }
    class Peer {
      ontrack: unknown = null;
      addTrack(): void {}
      createDataChannel(): Channel {
        const channel = new Channel();
        rt.channel = channel;
        return channel;
      }
      async createOffer(): Promise<{ type: string; sdp: string }> {
        return { type: "offer", sdp: "v=0\r\no=- desktop-offer\r\n" };
      }
      async setLocalDescription(): Promise<void> {}
      async setRemoteDescription(description: { sdp: string }): Promise<void> {
        rt.answer = description.sdp;
      }
      close(): void {
        rt.closed = true;
      }
    }
    (window as unknown as { RTCPeerConnection: unknown }).RTCPeerConnection = Peer;
  });
  const frames = received(page);
  const sockets = await openDesktop(page);
  const talk = page.locator("button[name=voice-chat]");
  await talk.click();
  const state = () => page.evaluate(() => (window as unknown as { __rt: Realtime }).__rt);
  await expect.poll(async () => (await state()).answer, { timeout: 30_000 }).toBe("v=0\r\no=- realtime-answer\r\n");
  const started = sent(sockets, "voice.realtime.start")[0]?.params as { sdp: string; session_id: string };
  expect(started.sdp).toContain("desktop-offer");

  // The model calls its one tool: the request runs as a turn of the chat.
  const model = (event: unknown) =>
    page.evaluate((e) => {
      const rt = (window as unknown as { __rt: { channel: { onmessage: (m: { data: string }) => void } } }).__rt;
      rt.channel.onmessage({ data: JSON.stringify(e) });
    }, event);
  await model({ type: "response.output_item.done", item: { type: "function_call", name: "prometheus_agent", call_id: "call_1", arguments: JSON.stringify({ request: "summarize notes.txt" }) } });
  await expect(page.locator(".transcript .tool[data-tool=read_file] .tool-state")).toHaveText("done", { timeout: 30_000 });
  await expect.poll(async () => (await state()).sent.map((e) => e.type), { timeout: 30_000 }).toEqual(["conversation.item.create", "response.create"]);
  const output = (await state()).sent[0]?.item;
  expect(output).toMatchObject({ type: "function_call_output", call_id: "call_1" });
  expect(output?.output).toBe("The notes say the launch is on Friday.");
  const turn = sent(sockets, "prompt.submit").map((f) => f.params as { session_id: string; text: string });
  expect(turn).toEqual([expect.objectContaining({ session_id: started.session_id, text: "summarize notes.txt" })]);

  // The response's tokens are reported for the usage ledger.
  await model({ type: "response.done", response: { id: "resp_1", usage: { input_tokens: 40, output_tokens: 25, input_token_details: { cached_tokens: 0 } } } });
  await expect.poll(() => sent(sockets, "voice.realtime.usage").length).toBe(1);

  await talk.click();
  await expect.poll(async () => (await state()).closed).toBe(true);
  // No provider key reached the page.
  const key = env("PMT_REALTIME_KEY");
  for (const frame of [...frames, ...sockets.flat()]) expect(frame).not.toContain(key);
});
