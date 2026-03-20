#!/usr/bin/env python3
"""Probe: what does vscode-markdown-language-server actually report?

Sends initialize, opens a markdown file, makes an edit, and logs:
  1. Server capabilities from the initialize response
  2. Every publishDiagnostics notification (with full params, including version if present)
  3. Any $/progress notifications

Usage: python3 tools/markdown_lsp_probe.py
"""

import json, os, subprocess, sys, tempfile, threading, time

responses = {}  # id -> response
responses_lock = threading.Lock()

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
                resp["result"] = [{}] * len(items)
                print(f"  [config request] sections: {[i.get('section','') for i in items]}")
            elif method == "window/workDoneProgress/create":
                token = msg.get("params", {}).get("token", "?")
                print(f"  [progress/create] token={token}")
                resp["result"] = None
            elif method == "client/registerCapability":
                regs = msg.get("params", {}).get("registrations", [])
                print(f"  [registerCapability] {[r.get('method','?') for r in regs]}")
                resp["result"] = None
            else:
                print(f"  [server request] {method}")
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
            print(f"\n  [publishDiagnostics]")
            print(f"    uri: {uri}")
            print(f"    version: {version!r}  (key present: {'version' in params})")
            print(f"    diagnostics: {len(diags)} items")
            for d in diags[:5]:
                sev = {1: "Error", 2: "Warning", 3: "Info", 4: "Hint"}.get(d.get("severity"), "?")
                print(f"      [{sev}] {d.get('message', '')[:100]}")
        elif method == "$/progress":
            v = msg.get("params", {}).get("value", {})
            print(f"  [progress] kind={v.get('kind')} title={v.get('title')!r} msg={v.get('message')!r}")
        elif method == "window/logMessage":
            text = msg.get("params", {}).get("message", "")
            if len(text) > 120: text = text[:120] + "..."
            print(f"  [logMessage] {text}")

def wait_response(req_id, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with responses_lock:
            if req_id in responses:
                return responses.pop(req_id)
        time.sleep(0.1)
    return None

def main():
    tmp = tempfile.mkdtemp(prefix="md_probe_")
    md_file = os.path.join(tmp, "test.md")
    with open(md_file, "w") as f:
        f.write("# Hello\n\nSome text.\n\n## Section\n\nMore text.\n")

    uri = f"file://{md_file}"
    print(f"workspace: {tmp}")
    print(f"file: {md_file}\n")

    proc = subprocess.Popen(
        ["vscode-markdown-language-server", "--stdio"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    threading.Thread(target=reader_thread, args=(proc,), daemon=True).start()
    time.sleep(0.3)

    # ── Initialize ──────────────────────────────────────────────────
    print("═══ Initialize ═══")
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
    if resp:
        caps = resp.get("result", {}).get("capabilities", {})
        print(f"\nServer capabilities:")
        print(json.dumps(caps, indent=2))

        info = resp.get("result", {}).get("serverInfo", {})
        if info:
            print(f"\nServer info: {json.dumps(info)}")
    else:
        print("ERROR: no initialize response")
        sys.exit(1)

    send(proc, "initialized", {})
    time.sleep(1)  # let config/registration requests arrive

    # ── Open file ───────────────────────────────────────────────────
    print("\n═══ didOpen ═══")
    with open(md_file) as f: text = f.read()
    send(proc, "textDocument/didOpen", {
        "textDocument": {"uri": uri, "languageId": "markdown", "version": 1, "text": text}
    })
    time.sleep(2)  # wait for initial diagnostics

    # ── Edit file (add a broken link) ──────────────────────────────
    print("\n═══ didChange (version 2 — add broken link) ═══")
    new_text = text + "\n[broken link](nonexistent.md)\n"
    send(proc, "textDocument/didChange", {
        "textDocument": {"uri": uri, "version": 2},
        "contentChanges": [{"text": new_text}]
    })
    time.sleep(2)

    # ── Another edit (add duplicate heading) ───────────────────────
    print("\n═══ didChange (version 3 — add duplicate heading) ═══")
    new_text2 = new_text + "\n## Section\n\nDuplicate heading.\n"
    send(proc, "textDocument/didChange", {
        "textDocument": {"uri": uri, "version": 3},
        "contentChanges": [{"text": new_text2}]
    })
    time.sleep(2)

    # ── Pull diagnostics (textDocument/diagnostic) ─────────────────
    print("\n═══ textDocument/diagnostic (pull model) ═══")
    send(proc, "textDocument/diagnostic", {
        "textDocument": {"uri": uri},
    }, req_id=200)
    resp = wait_response(200, timeout=5)
    if resp:
        result = resp.get("result", {})
        error = resp.get("error")
        if error:
            print(f"  error: {json.dumps(error)}")
        else:
            kind = result.get("kind", "?")
            result_id = result.get("resultId")
            items = result.get("items", [])
            print(f"  kind: {kind}")
            print(f"  resultId: {result_id!r}")
            print(f"  items: {len(items)} diagnostics")
            for d in items[:10]:
                sev = {1: "Error", 2: "Warning", 3: "Info", 4: "Hint"}.get(d.get("severity"), "?")
                rng = d.get("range", {})
                start = rng.get("start", {})
                print(f"    [{sev}] L{start.get('line', '?')}:{start.get('character', '?')} {d.get('message', '')[:100]}")
    else:
        print("  no response (timeout)")

    # ── Wait a bit more for any stragglers ─────────────────────────
    print("\n═══ Waiting 3s for late notifications... ═══")
    time.sleep(3)

    # ── Shutdown ───────────────────────────────────────────────────
    print("\n═══ Shutdown ═══")
    send(proc, "shutdown", None, req_id=9999)
    wait_response(9999, timeout=3)
    send(proc, "exit")
    try: proc.wait(timeout=3)
    except: proc.kill()

    print("\nDone.")

if __name__ == "__main__":
    main()
