"""The official A2A Python SDK against Prometheus's A2A server (task 74.1).

usage: a2a_sdk.py <base_url> <bearer_token>

Resolves the Agent Card, sends a text message without and with streaming,
fetches the task back and lists the caller's tasks, then prints one JSON
document with what the SDK parsed.
"""

import asyncio
import json
import sys
import uuid

import httpx
from a2a.client import ClientConfig, create_client
from a2a.types.a2a_pb2 import (
    GetTaskRequest,
    ListTasksRequest,
    Message,
    Part,
    Role,
    SendMessageRequest,
    TaskState,
)


def text_message(text, context_id=""):
    return SendMessageRequest(
        message=Message(
            message_id=str(uuid.uuid4()),
            role=Role.ROLE_USER,
            context_id=context_id,
            parts=[Part(text=text)],
        )
    )


def artifact_text(task):
    return "\n".join(part.text for art in task.artifacts for part in art.parts)


async def send(client, request):
    """Every event of one send; the last task the events carried."""
    task = None
    kinds = []
    async for event in client.send_message(request):
        kind = event.WhichOneof("payload")
        kinds.append(kind)
        if kind == "task":
            task = event.task
        elif kind == "status_update" and task is not None:
            task.status.CopyFrom(event.status_update.status)
        elif kind == "artifact_update" and task is not None:
            task.artifacts.append(event.artifact_update.artifact)
    return task, kinds


async def main():
    base_url, token = sys.argv[1], sys.argv[2]
    headers = {"Authorization": f"Bearer {token}"}
    report = {}
    async with httpx.AsyncClient(headers=headers, timeout=120) as http:
        blocking = await create_client(
            base_url, ClientConfig(httpx_client=http, streaming=False)
        )
        card = blocking._card  # the resolved card, as the SDK parsed it
        report["card"] = {
            "name": card.name,
            "url": card.supported_interfaces[0].url,
            "streaming": card.capabilities.streaming,
            "skills": [s.id for s in card.skills],
        }
        task, kinds = await send(blocking, text_message("hello from the sdk"))
        report["send"] = {
            "state": TaskState.Name(task.status.state),
            "reply": artifact_text(task),
            "context_id": task.context_id,
            "kinds": kinds,
        }
        fetched = await blocking.get_task(GetTaskRequest(id=task.id))
        report["get"] = {
            "state": TaskState.Name(fetched.status.state),
            "reply": artifact_text(fetched),
            "history": len(fetched.history),
        }
        streaming = await create_client(
            base_url, ClientConfig(httpx_client=http, streaming=True)
        )
        task, kinds = await send(
            streaming, text_message("stream to the sdk", task.context_id)
        )
        report["stream"] = {
            "state": TaskState.Name(task.status.state),
            "reply": artifact_text(task),
            "context_id": task.context_id,
            "kinds": kinds,
        }
        listed = await blocking.list_tasks(ListTasksRequest())
        report["list"] = {"total": listed.total_size}
    print(json.dumps(report))


asyncio.run(main())
