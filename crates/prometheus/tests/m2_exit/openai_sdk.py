"""The official OpenAI Python SDK against Prometheus's API server (task 60.4).

usage: openai_sdk.py <base_url> <api_key>

Prints one JSON document: the advertised models, a chat completion, a
streamed chat completion, and a run created twice with one idempotency key
(the SDK's generic POST, as a client of the Runs API would use it).
"""

import json
import sys
import time

import httpx
import openai


def main():
    base_url, api_key = sys.argv[1], sys.argv[2]
    client = openai.OpenAI(base_url=base_url, api_key=api_key, max_retries=0)
    models = [m.id for m in client.models.list().data]
    chat = client.chat.completions.create(
        model=models[0],
        messages=[{"role": "user", "content": "hello from the sdk"}],
    )
    stream = client.chat.completions.create(
        model=models[0],
        messages=[{"role": "user", "content": "stream to the sdk"}],
        stream=True,
    )
    chunks = [c.choices[0].delta.content or "" for c in stream if c.choices]
    body = {"input": "ticket work"}
    headers = {"Idempotency-Key": "sdk-ticket-123"}
    first = client.post("/runs", body=body, cast_to=httpx.Response, options={"headers": headers})
    retry = client.post("/runs", body=body, cast_to=httpx.Response, options={"headers": headers})
    run_id = first.json()["run_id"]
    run = {}
    for _ in range(1200):
        run = client.get(f"/runs/{run_id}", cast_to=object)
        if run["status"] in ("completed", "failed", "cancelled", "interrupted"):
            break
        time.sleep(0.1)
    print(json.dumps({
        "sdk": openai.__version__,
        "models": models,
        "chat": {
            "object": chat.object,
            "content": chat.choices[0].message.content,
            "finish_reason": chat.choices[0].finish_reason,
        },
        "streamed": {"chunks": len(chunks), "content": "".join(chunks)},
        "runs": {
            "first": {"status": first.status_code, "run_id": run_id},
            "retry": {
                "status": retry.status_code,
                "run_id": retry.json()["run_id"],
                "replayed": retry.headers.get("idempotency-replayed"),
            },
            "final": {"status": run.get("status"), "output": run.get("output")},
        },
    }))


if __name__ == "__main__":
    main()
