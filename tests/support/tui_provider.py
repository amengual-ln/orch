"""Synthetic providers for the PTY tests. Keep the filesystem state in each session cwd."""
import json
import os
from pathlib import Path
import sys
import time

args = sys.argv[1:]
agy = "--output-format" in args
model = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--model=")), None)
prompt = args[-1].removeprefix("--print=") if agy else args[-1]
if agy and prompt == "/model":
    assert "--conversation=tui-agy" in args
    print(json.dumps({"event": "command_result", "command": {
        "name": "model", "data": {"id": "fixture-agy-default", "label": "Fixture Agy (High)"},
    }}), flush=True)
    print(json.dumps({"event": "result", "result": {"status": "SUCCESS"}}), flush=True)
    sys.exit(0)

conversation = "tui-agy" if agy else "tui-codex"
if not agy and (codex_home := os.environ.get("ORCH_FAKE_CODEX_HOME")):
    conversation = "00000000-0000-4000-8000-000000000903"
    rollout = Path(codex_home) / "sessions" / "2026" / "11" / "28" / f"rollout-fixture-{conversation}.jsonl"
    rollout.parent.mkdir(parents=True, exist_ok=True)
    with rollout.open("a") as stream:
        stream.write(json.dumps({
            "type": "turn_context", "payload": {"model": model or "gpt-fixture-sol", "effort": "medium"},
        }) + "\n")

def event(**payload):
    print(json.dumps(payload), flush=True)


with Path("requests").open("a") as record:
    record.write(json.dumps({"args": args, "prompt": prompt, "cwd": os.getcwd(), "pid": os.getpid()}) + "\n")

if agy:
    details = {"cwd": os.getcwd(), "permission_mode": "strict"}
    if model is not None:
        details["model"] = model
    event(event="init", conversation_id=conversation, init=details)
else:
    event(type="thread.started", thread_id=conversation)
    event(type="turn.started")

if prompt == "wait":
    deadline = time.monotonic() + 25
    while not Path("continue").exists() and time.monotonic() < deadline:
        time.sleep(.02)
elif agy and prompt == "holi":
    print("synthetic stderr suggests --dangerously-skip-permissions", file=sys.stderr)
    event(event="result", result={
        "conversation_id": conversation,
        "status": "SUCCESS",
        "response": "",
        "denied_actions": [{"action": "read_file", "display_name": "ReadFile"}],
    })
    sys.exit(0)
elif prompt == "fail":
    if agy:
        event(event="result", result={"status": "ERROR", "error": "error de prueba"})
    else:
        event(type="error", message="error de prueba")
    sys.exit(1 if agy else 0)

text = "\n".join(f"LINE-{i:02}" for i in range(50)) if prompt == "long" else "RESPUESTA"
if agy:
    for delta in ("RESP", "UESTA"):
        event(event="step_update", step_update={
            "step_type": "agent_response", "step_index": 0, "text_delta": delta,
            "conversation_id": conversation,
        })
    event(event="result", result={"status": "SUCCESS", "response": text, "conversation_id": conversation})
else:
    event(type="item.completed", item={"type": "agent_message", "id": "m", "text": text})
    event(type="turn.completed")
