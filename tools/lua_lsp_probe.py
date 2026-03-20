#!/usr/bin/env python3
"""Probe: does lua-language-server's workspace ever finish loading?

Polls textDocument/hover every 2s watching for "Workspace loading" to
clear. Once ready (or after 30s), fires documentSymbol.

Usage: python3 tools/lua_lsp_probe.py
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
                result = []
                for item in items:
                    section = item.get("section", "")
                    if section == "Lua":
                        result.append({"workspace": {"library": []}, "runtime": {"version": "Lua 5.4"}})
                    else:
                        result.append({})
                resp["result"] = result
                print(f"    sections: {[i.get('section','') for i in items]}")
            elif method == "window/workDoneProgress/create":
                token = msg.get("params", {}).get("token", "?")
                print(f"  [progress/create] token={token}")
                resp["result"] = None
            elif method == "client/registerCapability":
                resp["result"] = None
            else:
                resp["result"] = None
            send_raw(proc.stdin, resp)
            continue

        # Store responses
        if msg_id is not None and not method:
            with responses_lock:
                responses[msg_id] = msg
            continue

        # Print notifications of interest
        if method == "$/progress":
            v = msg.get("params", {}).get("value", {})
            print(f"  [progress] kind={v.get('kind')} title={v.get('title')!r} pct={v.get('percentage')} msg={v.get('message')!r}")
        elif method == "textDocument/publishDiagnostics":
            uri = msg.get("params", {}).get("uri", "")
            diags = msg.get("params", {}).get("diagnostics", [])
            print(f"  [diagnostics] {uri.split('/')[-1]}: {len(diags)} items")

def wait_response(req_id, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with responses_lock:
            if req_id in responses:
                return responses.pop(req_id)
        time.sleep(0.1)
    return None

def main():
    tmp = tempfile.mkdtemp(prefix="lua_probe_")
    lua_file = os.path.join(tmp, "test.lua")
    with open(lua_file, "w") as f:
        f.write("local M = {}\nfunction M.setup(opts) M.opts = opts end\nfunction M.run() return true end\nreturn M\n")
    # Root marker so lua-language-server recognizes the workspace
    with open(os.path.join(tmp, ".luarc.json"), "w") as f:
        f.write("{}\n")

    uri = f"file://{lua_file}"
    print(f"workspace: {tmp}")

    proc = subprocess.Popen(["lua-language-server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    threading.Thread(target=reader_thread, args=(proc,), daemon=True).start()
    time.sleep(0.3)

    # Init
    send(proc, "initialize", {
        "processId": os.getpid(),
        "capabilities": {
            "textDocument": {"synchronization": {"didSave": True}, "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}, "publishDiagnostics": {"versionSupport": True}},
            "workspace": {"configuration": True, "workspaceFolders": True},
            "window": {"workDoneProgress": True},
        },
        "rootUri": f"file://{tmp}",
        "workspaceFolders": [{"uri": f"file://{tmp}", "name": "test"}],
    }, req_id=1)
    wait_response(1)
    send(proc, "initialized", {})
    time.sleep(1)  # let config requests arrive and auto-reply

    # Open file
    with open(lua_file) as f: text = f.read()
    send(proc, "textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": "lua", "version": 1, "text": text}})

    # Poll hover until workspace loading clears
    print("\nPolling hover for workspace loading status...")
    rid = 100
    ready = False
    for i in range(15):  # 30s max
        time.sleep(2)
        rid += 1
        send(proc, "textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 0, "character": 6}}, req_id=rid)
        resp = wait_response(rid, timeout=5)
        if resp:
            hover_text = resp.get("result", {})
            if hover_text and "contents" in hover_text:
                val = hover_text["contents"].get("value", "") if isinstance(hover_text["contents"], dict) else str(hover_text["contents"])
                print(f"  [{i*2:2d}s] hover: {val[:80]}")
                if "loading" not in val.lower():
                    ready = True
                    break
            else:
                print(f"  [{i*2:2d}s] hover: null/empty")
        else:
            print(f"  [{i*2:2d}s] hover: timeout")

    # Now try documentSymbol
    print(f"\nWorkspace ready: {ready}")
    print("Sending documentSymbol...")
    t0 = time.time()
    send(proc, "textDocument/documentSymbol", {"textDocument": {"uri": uri}}, req_id=999)
    resp = wait_response(999, timeout=15)
    elapsed = time.time() - t0

    if resp:
        result = resp.get("result", [])
        print(f"documentSymbol response in {elapsed:.3f}s: {len(result)} symbols")
        for sym in (result or [])[:10]:
            name = sym.get("name", "?")
            kind = sym.get("kind", "?")
            print(f"  {name} (kind={kind})")
    else:
        print(f"documentSymbol: NO RESPONSE after {elapsed:.1f}s")

    # Cleanup
    send(proc, "shutdown", None, req_id=9999)
    wait_response(9999, timeout=3)
    send(proc, "exit")
    try: proc.wait(timeout=3)
    except: proc.kill()

if __name__ == "__main__":
    main()
