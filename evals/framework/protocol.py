import json
import subprocess
import threading
import time
from typing import Any


class McpClient:
    def __init__(self, process: subprocess.Popen[bytes]):
        self.process = process
        self._request_id = 0
        self._lock = threading.Lock()
        self._reader_thread = threading.Thread(target=self._reader_loop, daemon=True)
        self._pending_responses: dict[int, Any] = {}
        self._response_events: dict[int, threading.Event] = {}
        self._closed = False
        self._reader_thread.start()

    def _write_stdin(self, payload: bytes) -> None:
        with self._lock:
            if not self.process.stdin or self.process.poll() is not None:
                raise RuntimeError("Subprocess is not running")
            self.process.stdin.write(payload)
            self.process.stdin.flush()

    def _reader_loop(self) -> None:
        while not self._closed and self.process.stdout:
            try:
                line = self.process.stdout.readline()
                if not line:
                    break
                line_str = line.decode("utf-8", errors="replace").strip()
                if not line_str:
                    continue
                try:
                    payload = json.loads(line_str)
                except json.JSONDecodeError:
                    continue

                req_id = payload.get("id")
                if req_id is not None:
                    with self._lock:
                        self._pending_responses[req_id] = payload
                        event = self._response_events.get(req_id)
                        if event:
                            event.set()
            except Exception:
                break

    def send_request(
        self, method: str, params: dict[str, Any] | None = None, timeout_s: float = 60.0
    ) -> dict[str, Any]:
        with self._lock:
            self._request_id += 1
            req_id = self._request_id
            event = threading.Event()
            self._response_events[req_id] = event

        payload = {
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params or {},
        }
        raw = json.dumps(payload) + "\n"

        self._write_stdin(raw.encode("utf-8"))

        if not event.wait(timeout=timeout_s):
            with self._lock:
                self._response_events.pop(req_id, None)
                self._pending_responses.pop(req_id, None)
            raise TimeoutError(f"Request {method} (id={req_id}) timed out after {timeout_s}s")

        with self._lock:
            self._response_events.pop(req_id, None)
            response = self._pending_responses.pop(req_id, {})
        if not isinstance(response, dict):
            return {}
        return response

    def initialize(
        self, client_name: str = "evals-bench-harness", timeout_s: float = 60.0
    ) -> dict[str, Any]:
        params = {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": client_name, "version": "1.0.0"},
        }
        resp = self.send_request("initialize", params, timeout_s=timeout_s)
        init_notif = {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }
        self._write_stdin((json.dumps(init_notif) + "\n").encode("utf-8"))
        return resp

    def list_tools(self, timeout_s: float = 60.0) -> list[dict[str, Any]]:
        response = self.send_request("tools/list", {}, timeout_s=timeout_s)
        if response.get("error"):
            raise RuntimeError(f"tools/list failed: {response['error']}")
        result = response.get("result")
        if not isinstance(result, dict):
            return []
        tools = result.get("tools")
        if not isinstance(tools, list):
            return []
        return [tool for tool in tools if isinstance(tool, dict)]

    def call_tool(
        self, tool_name: str, arguments: dict[str, Any], timeout_s: float = 60.0
    ) -> dict[str, Any]:
        params = {"name": tool_name, "arguments": arguments}
        start = time.perf_counter()
        response = self.send_request("tools/call", params, timeout_s=timeout_s)
        duration_ms = (time.perf_counter() - start) * 1000.0
        from .gates import raise_if_mcp_error

        raise_if_mcp_error(response)
        return {
            "response": response,
            "duration_ms": duration_ms,
        }

    def close(self) -> None:
        self._closed = True
        if self.process.poll() is None:
            try:
                self.process.terminate()
                self.process.wait(timeout=3.0)
            except Exception:
                self.process.kill()
