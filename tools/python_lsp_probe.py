#!/usr/bin/env python3
"""Probe: what does pyright-langserver actually report?

Sends initialize, opens a Python file, makes an edit, and logs:
  1. Server capabilities from the initialize response
  2. Every publishDiagnostics notification (with full params, including version if present)
  3. Any $/progress notifications
  4. Pull diagnostics (textDocument/diagnostic) if advertised

Usage: python3 tools/python_lsp_probe.py
"""

import json, os, subprocess, sys, tempfile, threading, time

t0 = time.time()
responses = {}  # id -> response
responses_lock = threading.Lock()

def ts():
    return f"{time.time() - t0:6.2f}s"

def log(msg):
    print(f"  [{ts()}] {msg}", flush=True)

def send_raw(stdin, msg):
    body = json.dumps(msg)
    stdin.write(f"Content-Length: {len(body)}\r\n\r\n{body}".encode())
    stdin.flush()

def send(proc, method, params=None, req_id=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if req_id is not None: msg["id"] = req_id
    if params is not None: msg["params"] = params
    send_raw(proc.stdin, msg)

def read_message(stdout):
    content_length = 0
    while True:
        line = stdout.readline()
        if not line: return None
        line = line.strip()
        if line == b"": break
        if line.startswith(b"Content-Length:"):
            content_length = int(line.split(b":")[1].strip())
    if content_length == 0: return None
    return json.loads(stdout.read(content_length))

def reader_thread(proc):
    while True:
        msg = read_message(proc.stdout)
        if msg is None: break
        method = msg.get("method", "")
        msg_id = msg.get("id")

        # Auto-respond to server requests
        if msg_id is not None and method:
            resp = {"jsonrpc": "2.0", "id": msg_id}
            if method == "workspace/configuration":
                items = msg.get("params", {}).get("items", [])
                result = []
                for item in items:
                    section = item.get("section", "")
                    if section == "python":
                        result.append({"pythonPath": "python3", "analysis": {"autoSearchPaths": True}})
                    elif section == "python.analysis":
                        result.append({"autoSearchPaths": True, "diagnosticMode": "openFilesOnly", "typeCheckingMode": "basic"})
                    elif section == "pyright":
                        result.append({})
                    else:
                        result.append({})
                resp["result"] = result
                log(f"config request: {[i.get('section','') for i in items]} → responded")
            elif method == "window/workDoneProgress/create":
                token = msg.get("params", {}).get("token", "?")
                log(f"progress/create: token={token}")
                resp["result"] = None
            elif method == "client/registerCapability":
                regs = msg.get("params", {}).get("registrations", [])
                log(f"registerCapability: {[r.get('method','?') for r in regs]}")
                resp["result"] = None
            else:
                log(f"server request: {method}")
                resp["result"] = None
            send_raw(proc.stdin, resp)
            continue

        # Store responses
        if msg_id is not None and not method:
            with responses_lock:
                responses[msg_id] = msg
            continue

        # Print notifications — full detail for publishDiagnostics
        if method == "textDocument/publishDiagnostics":
            params = msg.get("params", {})
            uri = params.get("uri", "")
            diags = params.get("diagnostics", [])
            version = params.get("version")
            log(f"publishDiagnostics:")
            log(f"  uri: {uri}")
            log(f"  version: {version!r}  (key present: {'version' in params})")
            log(f"  diagnostics: {len(diags)} items")
            for d in diags[:5]:
                sev = {1: "Error", 2: "Warning", 3: "Info", 4: "Hint"}.get(d.get("severity"), "?")
                log(f"    [{sev}] {d.get('message', '')[:100]}")
        elif method == "$/progress":
            v = msg.get("params", {}).get("value", {})
            log(f"progress: kind={v.get('kind')} title={v.get('title')!r} msg={v.get('message')!r}")
        elif method == "window/logMessage":
            text = msg.get("params", {}).get("message", "")
            if len(text) > 120: text = text[:120] + "..."
            log(f"logMessage: {text}")

def wait_response(req_id, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with responses_lock:
            if req_id in responses:
                return responses.pop(req_id)
        time.sleep(0.1)
    return None

def main():
    tmp = tempfile.mkdtemp(prefix="py_probe_")
    py_file = os.path.join(tmp, "test.py")
    with open(py_file, "w") as f:
        f.write("def greet(name: str) -> str:\n    return f\"Hello, {name}\"\n\nresult = greet(42)\n")

    uri = f"file://{py_file}"
    print(f"workspace: {tmp}", flush=True)
    print(f"file: {py_file}\n", flush=True)

    proc = subprocess.Popen(
        ["pyright-langserver", "--stdio"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    threading.Thread(target=reader_thread, args=(proc,), daemon=True).start()
    time.sleep(0.3)

    # ── Initialize ──────────────────────────────────────────────────
    print(f"═══ [{ts()}] Initialize ═══", flush=True)
    send(proc, "initialize", {
        "processId": os.getpid(),
        "capabilities": {
            "textDocument": {
                "synchronization": {"didSave": True, "dynamicRegistration": True},
                "publishDiagnostics": {"versionSupport": True},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
            },
            "workspace": {
                "configuration": True,
                "workspaceFolders": True,
            },
            "window": {"workDoneProgress": True},
        },
        "rootUri": f"file://{tmp}",
        "workspaceFolders": [{"uri": f"file://{tmp}", "name": "test"}],
    }, req_id=1)

    resp = wait_response(1)
    has_pull = False
    if resp:
        caps = resp.get("result", {}).get("capabilities", {})
        has_pull = "diagnosticProvider" in caps
        print(f"\nServer capabilities:", flush=True)
        print(json.dumps(caps, indent=2), flush=True)

        info = resp.get("result", {}).get("serverInfo", {})
        if info:
            print(f"\nServer info: {json.dumps(info)}", flush=True)
    else:
        print("ERROR: no initialize response", flush=True)
        sys.exit(1)

    send(proc, "initialized", {})
    print(f"\n═══ [{ts()}] Waiting for config/registration... ═══", flush=True)
    time.sleep(5)

    # ── Open file (has a type error: greet(42) but param is str) ──
    print(f"\n═══ [{ts()}] didOpen ═══", flush=True)
    with open(py_file) as f: text = f.read()
    send(proc, "textDocument/didOpen", {
        "textDocument": {"uri": uri, "languageId": "python", "version": 1, "text": text}
    })
    time.sleep(5)

    # ── Edit file (add another error) ──────────────────────────────
    print(f"\n═══ [{ts()}] didChange (version 2 — add undefined variable) ═══", flush=True)
    new_text = text + "\nprint(undefined_var)\n"
    send(proc, "textDocument/didChange", {
        "textDocument": {"uri": uri, "version": 2},
        "contentChanges": [{"text": new_text}]
    })
    time.sleep(5)

    # ── Pull diagnostics if advertised ─────────────────────────────
    if has_pull:
        print(f"\n═══ [{ts()}] textDocument/diagnostic (pull model) ═══", flush=True)
        send(proc, "textDocument/diagnostic", {
            "textDocument": {"uri": uri},
        }, req_id=200)
        resp = wait_response(200, timeout=5)
        if resp:
            result = resp.get("result", {})
            error = resp.get("error")
            if error:
                print(f"  error: {json.dumps(error)}", flush=True)
            else:
                kind = result.get("kind", "?")
                result_id = result.get("resultId")
                items = result.get("items", [])
                print(f"  kind: {kind}", flush=True)
                print(f"  resultId: {result_id!r}", flush=True)
                print(f"  items: {len(items)} diagnostics", flush=True)
                for d in items[:10]:
                    sev = {1: "Error", 2: "Warning", 3: "Info", 4: "Hint"}.get(d.get("severity"), "?")
                    rng = d.get("range", {})
                    start = rng.get("start", {})
                    print(f"    [{sev}] L{start.get('line', '?')}:{start.get('character', '?')} {d.get('message', '')[:100]}", flush=True)
        else:
            print("  no response (timeout)", flush=True)
    else:
        print(f"\n(server does not advertise diagnosticProvider — skipping pull request)", flush=True)

    # ── Wait a bit more for any stragglers ─────────────────────────
    print(f"\n═══ [{ts()}] Waiting 5s for late notifications... ═══", flush=True)
    time.sleep(5)

    # ── Shutdown ───────────────────────────────────────────────────
    print(f"\n═══ [{ts()}] Shutdown ═══", flush=True)
    send(proc, "shutdown", None, req_id=9999)
    wait_response(9999, timeout=3)
    send(proc, "exit")
    try: proc.wait(timeout=3)
    except: proc.kill()

    print(f"\n[{ts()}] Done.", flush=True)

if __name__ == "__main__":
    main()
