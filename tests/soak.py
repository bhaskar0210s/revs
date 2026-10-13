#!/usr/bin/env python3
"""Soak test: thousands of cursor moves, edits, resolves, reopenings and Previous/Next
commands against a real file. Reports RSS over time and CPU time, then (with HOLD=1)
leaves the server running for `leaks`.

usage: soak.py <revs binary> <file in a git repo> <scratch dir>
Edits are only sent over LSP; nothing is written to the repository. Snapshots go to the
scratch dir's home.
"""

import json
import os
import random
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import e2e  # noqa: E402  (reuses the LSP client)

BIN, FILE, WORK = (os.path.abspath(arg) for arg in sys.argv[1:4])
os.makedirs(os.path.join(WORK, "home"), exist_ok=True)
e2e.BIN = BIN
e2e.HOME = os.path.join(WORK, "home")
e2e.CLI_LOG = os.path.join(WORK, "zed-cli.log")
e2e.FAKE_CLI = os.path.join(WORK, "fake-zed-cli")
with open(e2e.FAKE_CLI, "w") as script:
    script.write(
        "#!/usr/bin/env python3\n"
        "import json, os, sys\n"
        f"open({e2e.CLI_LOG!r}, 'a').write(json.dumps({{'cwd': os.getcwd(), 'args': sys.argv[1:]}}) + '\\n')\n"
    )
os.chmod(e2e.FAKE_CLI, 0o755)


def rss_kb(pid):
    return int(subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip() or 0)


def cpu_seconds(pid):
    text = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    minutes, seconds = text.split(":")
    return int(minutes) * 60 + float(seconds)


def main():
    random.seed(7)
    repo = subprocess.run(["git", "-C", os.path.dirname(FILE), "rev-parse", "--show-toplevel"], capture_output=True, text=True).stdout.strip()
    client = e2e.Client()
    client.request("initialize", {"processId": os.getpid(), "rootUri": e2e.uri(repo), "capabilities": {}})
    client.notify("initialized", {})
    text = open(FILE).read()
    lines = text.count("\n")
    file_uri = e2e.uri(FILE)
    version = 1

    def hints(line, target=file_uri):
        return client.request("textDocument/inlayHint", {
            "textDocument": {"uri": target},
            "range": {"start": {"line": max(0, line - 40), "character": 0}, "end": {"line": line + 40, "character": 0}},
        })

    client.notify("textDocument/didOpen", {"textDocument": {"uri": file_uri, "languageId": "plaintext", "version": version, "text": text}})
    idle = rss_kb(client.process.pid)
    print(f"RSS with line blame off, file open: {idle / 1024:.1f} MB")

    # Line blame on, through the command (as from Zed's command selector).
    middle = lines // 2
    e2e.cursor(client, file_uri, middle)
    started = time.time()
    client.request("workspace/executeCommand", {"command": e2e.TOGGLE, "arguments": []})
    first = None
    for _ in range(400):
        result = hints(middle)
        if result:
            first = time.time() - started
            break
        client.pump(0.005)
    print(f"first annotation after toggling on: {first * 1000:.0f} ms" if first else "no annotation!")
    print("annotation:", e2e.hint_text(result[0]) if result else None)
    print(f"RSS after first blame: {rss_kb(client.process.pid) / 1024:.1f} MB")

    timings = []
    for round_index in range(5):
        for _ in range(400):
            line = random.randrange(lines)
            t0 = time.time()
            e2e.cursor(client, file_uri, line)
            hints(line)
            timings.append(time.time() - t0)
        for _ in range(100):
            line = random.randrange(lines)
            version += 1
            if random.random() < 0.5:
                change = {"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 0}}, "text": "// x\n"}
                lines += 1
            else:
                change = {"range": {"start": {"line": line, "character": 0}, "end": {"line": line + 1, "character": 0}}, "text": ""}
                lines -= 1
            client.notify("textDocument/didChange", {"textDocument": {"uri": file_uri, "version": version}, "contentChanges": [change]})
        for _ in range(20):
            line = random.randrange(lines)
            e2e.cursor(client, file_uri, line)
            result = hints(line)
            if result:
                client.request("inlayHint/resolve", result[0])
        client.notify("textDocument/didSave", {"textDocument": {"uri": file_uri}})
        e2e.cursor(client, file_uri, random.randrange(lines))
        client.pump(0.6)  # let the re-blame finish
        print(f"round {round_index + 1}: RSS {rss_kb(client.process.pid) / 1024:.1f} MB")

    # Previous/Next commands: from the working file, then inside the diffs they open.
    step_times = []
    slowest = []

    def step(command, target, line, character=4):
        e2e.cursor(client, target, line, character=character)
        calls, messages, prompts = len(e2e.cli_calls()), len(client.show_messages), len(client.prompts)
        t0 = time.time()
        client.request("workspace/executeCommand", {"command": command, "arguments": []})
        while time.time() - t0 < 10:
            client.pump(0.01)
            if len(e2e.cli_calls()) > calls or len(client.show_messages) > messages or len(client.prompts) > prompts:
                break
        step_times.append(time.time() - t0)
        slowest.append((time.time() - t0, command, target.rsplit("/", 1)[-1], line, character))
        new = e2e.cli_calls()[calls:]
        return e2e.diff_of(new[-1]) if new else None

    opened = []
    for _ in range(30):
        diff = step(e2e.PREVIOUS, file_uri, random.randrange(lines))
        if diff:
            opened.append(diff)
    for left, right, row, _ in opened[:15]:
        right_uri = e2e.uri(right)
        client.notify("textDocument/didOpen", {"textDocument": {"uri": right_uri, "languageId": "plaintext", "version": 1, "text": open(right).read()}})
        right_lines = max(1, open(right).read().count("\n"))
        for _ in range(4):
            step(random.choice([e2e.PREVIOUS, e2e.NEXT]), right_uri, random.randrange(right_lines), random.choice([0, 4]))
        client.notify("textDocument/didClose", {"textDocument": {"uri": right_uri}})
    client.pump(0.5)
    step_times.sort()
    print(f"{len(step_times)} Previous/Next steps: median {step_times[len(step_times) // 2] * 1000:.0f} ms, "
          f"max {step_times[-1] * 1000:.0f} ms (git + snapshot writing; Zed's drawing not included)")
    print(f"{len(client.prompts)} of them asked which line")
    for seconds, command, name, line, character in sorted(slowest, reverse=True)[:3]:
        print(f"  slow: {seconds * 1000:.0f} ms {command} {e2e.urllib.parse.unquote(name)} {line + 1}:{character}")
    print(f"RSS after commands: {rss_kb(client.process.pid) / 1024:.1f} MB")

    for _ in range(20):
        client.notify("textDocument/didClose", {"textDocument": {"uri": file_uri}})
        version += 1
        client.notify("textDocument/didOpen", {"textDocument": {"uri": file_uri, "languageId": "plaintext", "version": version, "text": text}})
        e2e.cursor(client, file_uri, random.randrange(text.count("\n")))
        client.pump(0.35)
    client.pump(1.0)

    timings.sort()
    print(f"cursor move + hint request: median {timings[len(timings) // 2] * 1000:.2f} ms, p99 {timings[int(len(timings) * 0.99)] * 1000:.2f} ms")
    print(f"final RSS {rss_kb(client.process.pid) / 1024:.1f} MB, server CPU {cpu_seconds(client.process.pid):.2f} s for {time.time() - started:.1f} s wall")
    print(f"PID {client.process.pid}")
    sys.stdout.flush()
    if os.environ.get("HOLD"):
        input("press enter to stop the server")
    client.request("shutdown", None)
    client.notify("exit", None)
    print("exit code", client.process.wait(timeout=5))


if __name__ == "__main__":
    main()
