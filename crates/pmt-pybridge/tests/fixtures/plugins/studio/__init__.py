"""Hermes-style image generation, text-to-speech and transcription providers."""

import base64
import os

from agent.image_gen_provider import ImageGenProvider
from agent.transcription_provider import TranscriptionProvider
from agent.tts_provider import TTSProvider

# A 1x1 PNG.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


class Pics(ImageGenProvider):
    @property
    def name(self) -> str:
        return "studio-pics"

    def generate(self, prompt, aspect_ratio="landscape", **kwargs):
        path = os.path.join(os.environ["HOME"], f"{aspect_ratio}.png")
        with open(path, "wb") as handle:
            handle.write(PNG)
        return {"success": True, "image": path, "model": "pics-1", "prompt": prompt,
                "aspect_ratio": aspect_ratio, "provider": self.name, "billing_source": "api_key:studio"}


class Voice(TTSProvider):
    @property
    def name(self) -> str:
        return "studio-voice"

    def synthesize(self, text, output_path, *, voice=None, model=None, speed=None, format="mp3", **extra):
        with open(output_path, "wb") as handle:
            handle.write(b"ID3" + text.encode())
        return output_path


class Ears(TranscriptionProvider):
    @property
    def name(self) -> str:
        return "studio-ears"

    def transcribe(self, file_path, *, model=None, language=None, **extra):
        size = os.path.getsize(file_path)
        return {"success": True, "transcript": f"heard {size} bytes in {language}", "provider": self.name}


def register(ctx):
    ctx.register_image_gen_provider(Pics())
    ctx.register_tts_provider(Voice())
    ctx.register_transcription_provider(Ears())
