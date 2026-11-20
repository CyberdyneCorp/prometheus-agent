// The realtime full-duplex voice chat (spec: voice-mode, Realtime full-
// duplex voice chat mode (upgrade)): with `voice.chat_mode = "realtime"`
// the conversation button connects the microphone to a realtime voice
// model over WebRTC. The daemon creates the call with the profile's key
// (`voice.realtime.start`: the page sends its SDP offer and gets the
// answer, never the key); the model listens, handles interruptions and
// speaks, and hands every substantive request to the agent through its one
// tool, which runs as an ordinary turn of this chat (its tools, grants and
// approvals as usual). Each response's tokens go to the usage ledger
// (`voice.realtime.usage`), apart from the agent's turns.

import type { VoiceRealtimeCall } from "../../../../schema/rpc";
import type { Talker, VoiceState } from "./conversation";
import type { VoiceLink } from "./speech";

/** A realtime provider event on the `oai-events` data channel. */
interface ProviderEvent {
  type: string;
  item?: { type?: string; name?: string; call_id?: string; arguments?: string };
  response?: { id?: string; usage?: { input_tokens?: number; output_tokens?: number; input_token_details?: { cached_tokens?: number } } };
}

export class RealtimeVoice {
  private peer: RTCPeerConnection | null = null;
  private stream: MediaStream | null = null;
  private readonly speaker = new Audio();
  private call: VoiceRealtimeCall | null = null;

  constructor(
    private readonly link: VoiceLink,
    private readonly talker: Talker & { session(): Promise<string> },
    private readonly show: (state: VoiceState) => void,
  ) {
    this.speaker.autoplay = true;
    this.speaker.dataset.voice = "realtime";
  }

  /** Connects; resolves once the call is up. */
  async start(): Promise<void> {
    const sessionId = await this.talker.session();
    const peer = new RTCPeerConnection();
    this.peer = peer;
    peer.ontrack = (event) => {
      this.speaker.srcObject = event.streams[0] ?? null;
    };
    this.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    for (const track of this.stream.getTracks()) peer.addTrack(track, this.stream);
    const events = peer.createDataChannel("oai-events");
    events.onmessage = (message: MessageEvent<string>) => void this.event(events, JSON.parse(message.data) as ProviderEvent);
    const offer = await peer.createOffer();
    await peer.setLocalDescription(offer);
    this.call = await this.link.call<VoiceRealtimeCall>("voice.realtime.start", { profile: this.link.profile, session_id: sessionId, sdp: offer.sdp ?? "" });
    await peer.setRemoteDescription({ type: "answer", sdp: this.call.sdp });
    this.show("listening");
  }

  /** Hangs up. */
  stop(): void {
    for (const track of this.stream?.getTracks() ?? []) track.stop();
    this.peer?.close();
    this.peer = null;
    this.stream = null;
    this.speaker.srcObject = null;
    this.call = null;
    this.show("idle");
  }

  private async event(channel: RTCDataChannel, event: ProviderEvent): Promise<void> {
    const call = this.call;
    if (!call) return;
    if (event.type === "response.output_item.done" && event.item?.type === "function_call" && event.item.name === call.tool) {
      await this.delegate(channel, event.item.call_id ?? "", event.item.arguments ?? "{}");
    } else if (event.type === "response.done" && event.response?.id && event.response.usage) {
      const usage = event.response.usage;
      await this.link
        .call("voice.realtime.usage", {
          profile: this.link.profile,
          call_id: call.call_id,
          response_id: event.response.id,
          usage: { input_tokens: usage.input_tokens ?? 0, output_tokens: usage.output_tokens ?? 0, cached_tokens: usage.input_token_details?.cached_tokens ?? 0 },
        })
        .catch((err: unknown) => console.warn("realtime usage not recorded:", err));
    }
  }

  /** The model asked the agent: the request runs as a turn of this chat
   *  and its answer goes back for the voice to speak. */
  private async delegate(channel: RTCDataChannel, callId: string, args: string): Promise<void> {
    let request = "";
    try {
      request = String((JSON.parse(args) as { request?: unknown }).request ?? "");
    } catch {
      request = args;
    }
    this.show("thinking");
    let output: string;
    try {
      output = (await this.talker.voiceTurn(request)) ?? "The agent finished without an answer.";
    } catch (err) {
      output = `The agent could not run the request: ${(err as Error).message}`;
    }
    if (channel.readyState !== "open") return;
    channel.send(JSON.stringify({ type: "conversation.item.create", item: { type: "function_call_output", call_id: callId, output } }));
    channel.send(JSON.stringify({ type: "response.create" }));
    this.show("speaking");
  }
}
