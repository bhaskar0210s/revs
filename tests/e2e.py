#!/usr/bin/env python3
"""End-to-end test: drives `revs lsp` like Zed does, against tests/make_repo.sh's repo.

usage: e2e.py <revs binary> <scratch dir>
A fake Zed CLI records every diff/file the server asks Zed to open.

History of src/app.txt (src/old_name.txt before the rename):
  c1 Add file (#1)            one two three four five          (root commit)
  c2 Shout two                one TWO three four five
  c3 Rename to app.txt        (same text, renamed)
  c4 Insert line and add six  one TWO three inserted four five six   (Other Dev)
  c5 Shout one                ONE TWO three inserted four five six
  c6 Add CRLF file            (src/crlf.txt only)
  c7 Drop three and inserted  ONE TWO four five six
  c8 Drop five                ONE TWO four six                       (HEAD)
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time
import urllib.parse

BIN = os.path.abspath(sys.argv[1])
WORK = os.path.abspath(sys.argv[2])
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.join(WORK, "repo")
HOME = os.path.join(WORK, "home")
CACHE = os.path.join(HOME, "Library/Caches/Revs")
CLI_LOG = os.path.join(WORK, "zed-cli.log")
FAKE_CLI = os.path.join(WORK, "fake-zed-cli")

# Zero-width spaces after "Revs:" make Zed's sorted list show them in this order.
PREVIOUS = "Revs: Previous Revision"
NEXT = "Revs:\u200b Next Revision"
OPEN_FILE = "Revs:\u200b\u200b Open Working File"
OPEN_COMMIT = "Revs:\u200b\u200b\u200b Open Commit on GitHub"
TOGGLE = "Revs:\u200b\u200b\u200b\u200b Toggle Line Blame"

failures = []


def check(condition, message):
    if condition:
        print("  ok  ", message)
    else:
        print("  FAIL", message)
        failures.append(message)


def git(*args):
    return subprocess.run(["git", *args], cwd=REPO, check=True, capture_output=True, text=True).stdout.strip()


def setup():
    os.makedirs(HOME, exist_ok=True)
    subprocess.run([os.path.join(HERE, "make_repo.sh"), REPO], check=True)
    with open(FAKE_CLI, "w") as f:
        f.write(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            f"open({CLI_LOG!r}, 'a').write(json.dumps({{'cwd': os.getcwd(), 'args': sys.argv[1:]}}) + '\\n')\n"
        )
    os.chmod(FAKE_CLI, 0o755)
    if os.path.exists(CLI_LOG):
        os.remove(CLI_LOG)


def cli_calls():
    if not os.path.exists(CLI_LOG):
        return []
    return [json.loads(line) for line in open(CLI_LOG)]


def env():
    environment = dict(os.environ)
    environment["HOME"] = HOME
    environment["REVS_ZED_CLI"] = FAKE_CLI
    return environment


class Client:
    def __init__(self):
        self.process = subprocess.Popen(
            [BIN, "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env(),
        )
        self.messages = queue.Queue()
        self.next_id = 1000
        self.refreshes = []
        self.show_documents = []
        self.show_messages = []
        self.prompts = []
        self.answer = None  # title to pick for window/showMessageRequest
        self.opened = set()  # documents this client has sent didOpen for
        self.last_report = (None, 0.0)  # (uri, time) of the last cursor report
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        stream = self.process.stdout
        while True:
            length = None
            while True:
                line = stream.readline()
                if not line:
                    return
                line = line.decode().strip()
                if not line:
                    break
                name, value = line.split(":", 1)
                if name.lower() == "content-length":
                    length = int(value)
            body = json.loads(stream.read(length))
            self.messages.put(body)

    def _write(self, message):
        body = json.dumps(message).encode()
        self.process.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.process.stdin.flush()

    def notify(self, method, params):
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def _handle_server_message(self, message):
        method = message.get("method")
        if method in ("workspace/inlayHint/refresh", "workspace/codeLens/refresh"):
            self.refreshes.append(method)
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "window/showDocument":
            self.show_documents.append(message["params"])
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": {"success": True}})
        elif method == "window/showMessage":
            self.show_messages.append(message["params"]["message"])
        elif method == "window/showMessageRequest":
            self.prompts.append(message["params"])
            titles = [action["title"] for action in message["params"].get("actions", [])]
            pick = next((title for title in titles if self.answer and self.answer in title), None)
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": {"title": pick} if pick else None})
        elif "id" in message and method:
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": None})

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            try:
                message = self.messages.get(timeout=max(0.0, end - time.time()))
            except queue.Empty:
                return
            self._handle_server_message(message)

    def request(self, method, params, timeout=10):
        self.next_id += 1
        request_id = self.next_id
        self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        end = time.time() + timeout
        while time.time() < end:
            message = self.messages.get(timeout=max(0.01, end - time.time()))
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message.get("result")
            self._handle_server_message(message)
        raise TimeoutError(method)


def uri(path):
    return "file://" + urllib.parse.quote(path)


def text_of(path):
    return open(path, newline="").read()


def hint_text(hint):
    label = hint["label"]
    return label if isinstance(label, str) else "".join(part["value"] for part in label)


def hints(client, file_uri, start=0, end=50):
    return client.request(
        "textDocument/inlayHint",
        {
            "textDocument": {"uri": file_uri},
            "range": {"start": {"line": start, "character": 0}, "end": {"line": end, "character": 0}},
        },
    )


def wait_for_hint(client, file_uri, line, lines=50):
    for _ in range(60):
        found = hints(client, file_uri, 0, lines)
        if found and found[0]["position"]["line"] == line:
            return found[0]
        client.pump(0.05)
    return None


def space_reports(client, file_uri):
    """Clicks in two files are always well apart; reports closer than 100 ms are one of
    Zed's refresh bursts to Revs."""
    last_uri, at = client.last_report
    if last_uri is not None and last_uri != file_uri:
        time.sleep(max(0.0, 0.15 - (time.time() - at)))
    client.last_report = (file_uri, time.time())


def cursor(client, file_uri, line, character=1, selected=False):
    """What Zed sends ~250 ms after the cursor settles."""
    space_reports(client, file_uri)
    start = {"line": line, "character": character}
    end = {"line": line, "character": character + (3 if selected else 0)}
    return client.request(
        "textDocument/codeAction",
        {"textDocument": {"uri": file_uri}, "range": {"start": start, "end": end}, "context": {"diagnostics": []}},
    )


def run(client, command, *arguments, wait=3.0):
    """Like Zed's LSP command selector: no arguments unless given. Waits until the command
    opened a diff or said something (or `wait` seconds passed)."""
    calls, messages = len(cli_calls()), len(client.show_messages)
    result = client.request("workspace/executeCommand", {"command": command, "arguments": list(arguments)})
    if result is not None:
        failures.append(f"{command}: should answer null at once")
    deadline = time.time() + wait
    while time.time() < deadline:
        client.pump(0.05)
        if len(cli_calls()) > calls or len(client.show_messages) > messages:
            client.pump(0.1)
            break
    new_calls = cli_calls()[calls:]
    new_messages = client.show_messages[messages:]
    return (new_calls[-1] if new_calls else None), new_messages


def diff_of(call):
    """(left path, right path, 1-based row, 1-based column) from a `cli -e [file] --diff`
    call."""
    args = call["args"]
    assert args[0] == "-e" and "--diff" in args, args
    index = args.index("--diff")
    right, row, column = args[index + 2].rsplit(":", 2)
    return args[index + 1], right, int(row), int(column)


def sidecar(right):
    folder, name = os.path.split(right)
    return json.load(open(os.path.join(folder, "." + name + ".json")))


def open_doc(client, path, text=None):
    client.opened.add(uri(path))
    client.notify("textDocument/didOpen", {"textDocument": {
        "uri": uri(path), "languageId": "plaintext", "version": 1,
        "text": text if text is not None else text_of(path).replace("\r\n", "\n")}})


def land(client, call):
    """What Zed does after `cli --diff`: opens the right side (no didOpen when that buffer
    is open already) and reports the cursor where the CLI put it."""
    _, right, row, column = diff_of(call)
    if uri(right) not in client.opened:
        open_doc(client, right)
    cursor(client, uri(right), row - 1, character=column - 1)
    client.pump(0.05)
    return uri(right)


def select(client, file_uri, first, last):
    """A selection from the start of `first` to the end of `last` (as Zed reports it)."""
    space_reports(client, file_uri)
    return client.request(
        "textDocument/codeAction",
        {"textDocument": {"uri": file_uri}, "range": {"start": {"line": first, "character": 0},
         "end": {"line": last + 1, "character": 0}}, "context": {"diagnostics": []}},
    )


def main():
    setup()
    shas = git("log", "--format=%H", "--reverse").split()
    c = {index + 1: sha for index, sha in enumerate(shas)}
    short = {index: sha[:7] for index, sha in c.items()}
    app = os.path.join(REPO, "src/app.txt")
    app_uri = uri(app)
    snapshots = os.path.join(CACHE, "repos/repo/src")

    def left_file(name, label):
        return os.path.join(snapshots, "." + name, label)

    def right_file(name, label):
        return os.path.join(snapshots, f"{label} {name}")

    client = Client()

    print("initialize")
    result = client.request("initialize", {"processId": os.getpid(), "rootUri": uri(REPO), "capabilities": {}})
    caps = result["capabilities"]
    commands = caps["executeCommandProvider"]["commands"]
    check(commands == [PREVIOUS, NEXT, OPEN_FILE, OPEN_COMMIT, TOGGLE], f"readable command names: {commands}")
    check(sorted(commands) == commands, "Zed's sorted list shows them in menu order")
    check("codeLensProvider" not in caps, "no code lenses")
    check("linkedEditingRangeProvider" not in caps, "linked editing left to other servers")
    check(caps["codeActionProvider"] == {"codeActionKinds": []}, "code action requests at the cursor only")
    client.notify("initialized", {})

    print("line blame is off until toggled")
    open_doc(client, app)
    check(cursor(client, app_uri, 1) == [], "no code actions offered")
    client.pump(0.2)
    check(hints(client, app_uri) == [], "no annotation by default")
    check(not client.refreshes, "no hint refreshes while off")
    refreshes = len(client.refreshes)
    client.notify("workspace/didChangeConfiguration", {"settings": {}})
    client.notify("workspace/didChangeConfiguration", {"settings": {}})
    client.pump(0.2)
    check(len(client.refreshes) == refreshes, "repeated configuration notices cause no refreshes")

    print("Previous Revision in the working file (no arguments, last cursor)")
    call, messages = run(client, PREVIOUS)
    check(call is not None, f"diff opened {messages}")
    if call:
        left, right, row, column = diff_of(call)
        check(os.path.realpath(call["cwd"]) == os.path.realpath(REPO), "CLI runs in the project root")
        check(left == left_file("old_name.txt", short[1]), f"left is c1, named by its id: {left}")
        check(right == right_file("old_name.txt", short[2]), f"right is c2 under its old name: {right}")
        check((row, column) == (2, 1), f"cursor on the line: {row}:{column}")
        check(text_of(left) == "one\ntwo\nthree\nfour\nfive\n" and text_of(right) == "one\nTWO\nthree\nfour\nfive\n", "snapshot contents")
        check(oct(os.stat(right).st_mode & 0o777) == "0o444", "snapshots are read-only")
        pair = sidecar(right)
        check(pair["left"]["rev"] == c[1] and pair["right"]["rev"] == c[2] and pair["file"] == "src/app.txt", "pair record")
        check(pair["right"]["summary"] == "Shout two", "pair records the commit")
    card = messages[-1] if messages else ""
    check(f"**{short[2]}** Shout two" in card and "from app.txt line 2" in card, f"card names the commit, file and line: {card!r}")
    check(f"[GitHub](https://github.com/example/project/commit/{c[2]})" in card, "card links the commit")

    cursor(client, app_uri, 3)  # "six", added by c4
    call, _ = run(client, PREVIOUS)
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("app.txt", short[3]) and right == right_file("app.txt", short[4]), "six: c3 ↔ c4")
        check(row == 7, f"at six in c4 (row {row})")
    _, messages = run(client, NEXT)
    check(messages and "nothing newer" in messages[-1], f"next in the working file: {messages}")

    print("a selection: the newest change among its lines")
    select(client, app_uri, 0, 1)  # ONE (c5) and TWO (c2)
    call, messages = run(client, PREVIOUS)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == right_file("app.txt", short[5]) and row == 1, f"lines 1-2: c5 ({right}, row {row})")
    check(messages and "from app.txt lines 1–2" in messages[-1], "card names the selection")

    print("diff c3 ↔ c4: right side")
    view34 = right_file("app.txt", short[4])
    view34_uri = uri(view34)
    open_doc(client, view34)
    cursor(client, view34_uri, 6)  # six
    call, messages = run(client, PREVIOUS)
    check(call is None and messages and "added in" in messages[-1], f"six was added in c4: no earlier version ({messages})")
    cursor(client, view34_uri, 1)  # TWO, unchanged in c4
    view12 = right_file("old_name.txt", short[2])
    call, _ = run(client, PREVIOUS)
    check(call is not None, "TWO: a diff opens")
    if call:
        _, right, row, _ = diff_of(call)
        check(right == view12 and row == 2, "TWO: c1 ↔ c2")
        land(client, call)
    # Right back: Next from where Previous put the cursor reopens the diff it came from.
    call, messages = run(client, NEXT)
    check(call is not None, "next after previous opens a diff")
    if call:
        left, right, row, _ = diff_of(call)
        check(right == view34 and row == 2, f"next after previous returns to c3 ↔ c4 at TWO ({right}, row {row})")
        check(messages and "back to app.txt line 2" in messages[-1], f"card says it went back: {messages}")
        land(client, call)

    print("a return trail belongs to the step that made it")
    view12_uri = uri(view12)
    cursor(client, view12_uri, 0, character=2)  # in c1 ↔ c2, click "one"
    call, _ = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == right_file("old_name.txt", short[1]), "one: its commit c1")
    if call:
        land(client, call)
    call, _ = run(client, NEXT)  # straight back to c1 ↔ c2, now at "one"
    check(call is not None and diff_of(call)[1] == view12 and diff_of(call)[2] == 1, "back to c1 ↔ c2 at one")
    if call:
        land(client, call)
    call, messages = run(client, NEXT)  # one's own next change, not the diff before c1 ↔ c2
    check(call is not None and diff_of(call)[1] == right_file("app.txt", short[5]) and diff_of(call)[2] == 1,
          f"next from there follows one to c4 ↔ c5: {call and diff_of(call)} {messages}")

    print("a diff reached again from elsewhere takes the new route")
    cursor(client, app_uri, 1)  # TWO in the working file
    call, _ = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == view12, "the same c1 ↔ c2 again")
    if call:
        land(client, call)  # already open: Zed shares the buffer, no didOpen
    call, messages = run(client, NEXT)
    check(call is None and messages and "No later changes" in messages[-1], f"no return to an older route: {messages}")
    cursor(client, view34_uri, 0)  # one, from the root commit
    call, messages = run(client, PREVIOUS)
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("old_name.txt", "∅") and right == right_file("old_name.txt", short[1]), "root commit vs. empty")
        check(text_of(left) == "", "empty left side")
    check(any("first commit" in message for message in messages), f"root commit noted: {messages}")
    cursor(client, view34_uri, 3)  # inserted: next change removes it (c7)
    call, messages = run(client, NEXT)
    view67 = right_file("app.txt", short[7])
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("app.txt", short[6]) and right == view67, f"inserted: next is c6 ↔ c7 ({left}, {right})")
        check(row == 2, f"cursor on the line above the removal (row {row})")
    check(any("removed" in message for message in messages), f"removal noted: {messages}")
    view67_uri = uri(view67)
    open_doc(client, view67)  # cursor where Revs put it (from the record); the removed line is the focus
    _, messages = run(client, NEXT)
    check(messages and "removed in" in messages[-1], f"next again: the line ends here ({messages})")
    cursor(client, view34_uri, 0, character=2)  # one: changed next by c5
    call, _ = run(client, NEXT)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == right_file("app.txt", short[5]) and row == 1, "one: next is c4 ↔ c5")
    cursor(client, view34_uri, 1)
    run(client, OPEN_COMMIT)
    shown = client.show_documents[-1] if client.show_documents else {}
    check(shown.get("external") is True and shown.get("uri") == f"https://github.com/example/project/commit/{c[4]}", f"open commit on GitHub: {shown.get('uri')}")

    print("a diff reopened by another Revs process takes that route")
    cursor(client, view34_uri, 1)  # TWO in c3 ↔ c4
    call, _ = run(client, PREVIOUS)  # c1 ↔ c2, with the way back to c3 ↔ c4
    if call:
        land(client, call)
    subprocess.run([BIN, "previous", app, "2"], env=env(), check=True, cwd=REPO)  # reopened without one
    land(client, cli_calls()[-1])  # a new tab on the same buffer: no didOpen
    call, messages = run(client, NEXT)
    check(call is None and messages and "No later changes" in messages[-1], f"the reopened route counts: {messages}")

    print("diff c6 ↔ c7: removed lines from two commits")
    cursor(client, view67_uri, 2, character=0)  # a click on a removed left line
    client.answer = "inserted"
    call, _ = run(client, PREVIOUS, wait=3.0)
    prompt = client.prompts[-1] if client.prompts else {}
    titles = [action["title"] for action in prompt.get("actions", [])]
    check(titles == [f"4: inserted — {short[4]}", f"3: three — {short[1]}"], f"asked which line, newest first: {titles}")
    if call:
        _, right, row, _ = diff_of(call)
        check(right == view34 and row == 4, "picked: c3 ↔ c4 at inserted")
    cursor(client, view67_uri, 2, character=0)  # back on the removed lines
    _, messages = run(client, NEXT)
    check(messages and "removed in" in messages[-1], f"next on a removed line: {messages}")
    cursor(client, view67_uri, 2, character=2)  # the right line "four" itself
    call, _ = run(client, PREVIOUS)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == right_file("old_name.txt", short[1]) and row == 4, "four (right side): its commit c1")
    cursor(client, view67_uri, 3)  # five: removed next by c8
    call, messages = run(client, NEXT)
    view78 = right_file("app.txt", short[8])
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("app.txt", short[7]) and right == view78 and row == 3, f"five: next is c7 ↔ c8, cursor above the removal ({row})")

    print("diff c7 ↔ c8: one removed line")
    view78_uri = uri(view78)
    open_doc(client, view78)  # focus: the removed "five"
    call, _ = run(client, PREVIOUS)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == view67 and row == 4, f"previous right after next returns to c6 ↔ c7 at five ({right}, {row})")
    cursor(client, view78_uri, 1)  # the user moves elsewhere...
    cursor(client, view78_uri, 3, character=0)  # ...then clicks the removed line on the left
    call, _ = run(client, PREVIOUS)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == right_file("old_name.txt", short[1]) and row == 5, f"five: its commit c1 at row 5 ({row})")

    print("closing a tab falls back to the previous one")
    cursor(client, app_uri, 1)
    cursor(client, view78_uri, 2, character=1)
    client.notify("textDocument/didClose", {"textDocument": {"uri": view78_uri}})
    call, _ = run(client, PREVIOUS)
    if call:
        _, right, _, _ = diff_of(call)
        check(right == right_file("old_name.txt", short[2]), f"acts on the working file's line again ({right})")

    print("uncommitted changes")
    cursor(client, app_uri, 2, character=0)  # the user clicks line 3 and types
    client.notify("textDocument/didChange", {
        "textDocument": {"uri": app_uri, "version": 2},
        "contentChanges": [{"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 4}}, "text": "FOUR!"}],
    })
    cursor(client, app_uri, 2)
    cursor(client, app_uri, 0)  # the same file's other pane reports its cursor right after the edit
    call, _ = run(client, PREVIOUS)
    working_view = right_file("app.txt", "Working")
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("app.txt", short[8]) and right == working_view, "the typing pane's line wins: HEAD ↔ working")
        check("FOUR!" in text_of(right) and row == 3, "working side has the unsaved text")
    open_doc(client, working_view)
    cursor(client, uri(working_view), 2)
    call, _ = run(client, PREVIOUS)
    if call:
        _, right, row, _ = diff_of(call)
        check(right == right_file("old_name.txt", short[1]) and row == 4, "changed working line: the old line's commit")
    cursor(client, uri(working_view), 2)
    _, messages = run(client, NEXT)
    check(messages and "nothing newer" in messages[-1], "next on the working side")
    cursor(client, view34_uri, 4, character=2)  # four in c4, changed only in the working tree
    call, _ = run(client, NEXT)
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("app.txt", short[8]) and right == working_view and row == 3, f"next reaches the working tree ({right}, {row})")

    print("Open Working File from a diff")
    cursor(client, view67_uri, 4, character=1)  # six in c7
    run(client, OPEN_FILE)
    shown = client.show_documents[-1] if client.show_documents else {}
    check(shown.get("uri") == app_uri and shown.get("external") is False, "opens the working file")
    check(shown.get("selection", {}).get("start", {}).get("line") == 3, f"at six's line there: {shown.get('selection')}")
    cursor(client, view12_uri, 1, character=1)  # TWO in c2, when the file was still old_name.txt
    run(client, OPEN_FILE)
    shown = client.show_documents[-1] if client.show_documents else {}
    check(shown.get("uri") == app_uri and shown.get("selection", {}).get("start", {}).get("line") == 1,
          f"follows the line across the rename: {shown.get('uri')} {shown.get('selection')}")
    cursor(client, app_uri, 1)
    _, messages = run(client, OPEN_FILE)
    check(messages and "already the working file" in messages[-1], "open file in the working file")

    print("a click well after typing counts; echoes from other panes don't")
    def touch_line_three():
        client.notify("textDocument/didChange", {
            "textDocument": {"uri": app_uri, "version": 3},
            "contentChanges": [{"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 3}}, "text": "six"}],
        })
    touch_line_three()
    cursor(client, app_uri, 3)  # the pane that typed
    cursor(client, app_uri, 0)  # another pane, at once: an echo
    call, _ = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == right_file("app.txt", short[4]) and diff_of(call)[2] == 7, "echo ignored: six")
    cursor(client, app_uri, 3, character=0)  # back in app.txt, typing on six again
    touch_line_three()
    cursor(client, app_uri, 3)
    time.sleep(0.25)
    cursor(client, app_uri, 0)  # a real click a moment later
    call, _ = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == right_file("app.txt", short[5]) and diff_of(call)[2] == 1, "late click counts: ONE")

    print("CRLF file")
    crlf = os.path.join(REPO, "src/crlf.txt")
    open_doc(client, crlf)
    cursor(client, uri(crlf), 1)
    call, messages = run(client, PREVIOUS)
    if call:
        left, right, row, _ = diff_of(call)
        check(left == left_file("crlf.txt", "∅") and right == right_file("crlf.txt", short[6]) and row == 2, "CRLF line blamed to c6")
        check(text_of(right) == "first\r\nsecond\r\n", "CRLF kept in the snapshot")
    check(any("added here" in message for message in messages), f"file creation noted: {messages}")

    print("files git doesn't know yet")
    fresh = os.path.join(REPO, "src/fresh.txt")
    open(fresh, "w").write("new\n")
    open_doc(client, fresh)
    cursor(client, uri(fresh), 0)
    _, messages = run(client, PREVIOUS)
    check(messages and messages[-1] == "This file isn't committed yet", f"untracked file: {messages}")
    git("add", "src/fresh.txt")
    _, messages = run(client, PREVIOUS)
    check(messages and messages[-1] == "This file isn't committed yet", f"staged new file: {messages}")
    git("rm", "-q", "--cached", "src/fresh.txt")
    os.remove(fresh)

    print("typing after a tab switch Zed doesn't report")
    cursor(client, app_uri, 1)  # app.txt's cursor on TWO
    cursor(client, uri(crlf), 1)  # then the user works in crlf.txt, and switches back to app.txt by tab
    client.notify("textDocument/didChange", {  # pastes three lines at the cursor
        "textDocument": {"uri": app_uri, "version": 6},
        "contentChanges": [{"range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 1}}, "text": "a\nb\nc\n"}],
    })
    cursor(client, app_uri, 4, character=0)  # the cursor after the paste, the only report
    call, messages = run(client, PREVIOUS)
    check(messages and "from app.txt line 5" in messages[-1], f"the paste's tab is the target: {messages}")
    client.notify("textDocument/didChange", {  # undo the paste
        "textDocument": {"uri": app_uri, "version": 7},
        "contentChanges": [{"range": {"start": {"line": 1, "character": 1}, "end": {"line": 4, "character": 0}}, "text": ""}],
    })
    time.sleep(0.75)

    print("a refresh burst (a language server starting) changes nothing")
    cursor(client, uri(crlf), 1)  # the user's click
    time.sleep(0.15)
    client.last_report = (None, 0.0)  # the burst comes all at once:
    client.request("textDocument/codeAction", {"textDocument": {"uri": app_uri},
        "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 1}}, "context": {"diagnostics": []}})
    client.request("textDocument/codeAction", {"textDocument": {"uri": uri(crlf)},
        "range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 1}}, "context": {"diagnostics": []}})
    client.request("textDocument/codeAction", {"textDocument": {"uri": app_uri},
        "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 1}}, "context": {"diagnostics": []}})
    time.sleep(0.15)
    call, messages = run(client, PREVIOUS)
    check(messages and "from crlf.txt line 2" in messages[-1], f"still the clicked line: {messages}")

    print("reports that aren't the user")
    cursor(client, app_uri, 3)
    cursor(client, uri(crlf), 1)  # the user is in crlf.txt
    client.notify("textDocument/didChange", {  # app.txt changes on disk (a pull), in the background
        "textDocument": {"uri": app_uri, "version": 5},
        "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "text": "ONE"}],
    })
    cursor(client, app_uri, 3)  # ...and its tab re-reports its cursor
    client.request("textDocument/codeAction", {"textDocument": {"uri": app_uri},
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 4, "character": 0}},
        "context": {"diagnostics": [], "only": ["source"]}})  # an on-save request for the whole file
    call, messages = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == right_file("crlf.txt", short[6]), "still crlf.txt line 2")
    check(messages and "from crlf.txt line 2" in messages[-1], f"card: {messages}")
    time.sleep(0.7)
    cursor(client, app_uri, 3)  # a click there a moment later counts
    call, _ = run(client, PREVIOUS)
    check(call is not None and diff_of(call)[1] == right_file("app.txt", short[4]), "then app.txt line 4")

    print("a background tab reloaded right at its cursor")
    cursor(client, uri(crlf), 1)  # crlf.txt's cursor on 'second'
    cursor(client, app_uri, 1)  # the user is in app.txt
    open(crlf, "w", newline="").write("first\r\nsECOND\r\n")  # changed on disk...
    client.notify("textDocument/didChange", {  # ...and reloaded, starting exactly at its cursor
        "textDocument": {"uri": uri(crlf), "version": 30},
        "contentChanges": [{"range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 6}}, "text": "ECOND"}],
    })
    cursor(client, uri(crlf), 1)  # its re-report
    call, messages = run(client, PREVIOUS)
    check(messages and "from app.txt line 2" in messages[-1], f"still app.txt: {messages}")
    git("checkout", "--", "src/crlf.txt")
    client.notify("textDocument/didChange", {
        "textDocument": {"uri": uri(crlf), "version": 31},
        "contentChanges": [{"range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 6}}, "text": "econd"}],
    })
    time.sleep(0.75)

    print("toggle line blame")
    cursor(client, app_uri, 1)
    run(client, TOGGLE, wait=0.3)
    state = json.load(open(os.path.join(HOME, "Library/Application Support/Revs/state.json")))
    check(state == {"lineBlame": True}, "toggle remembered")
    hint = wait_for_hint(client, app_uri, 1)
    check(hint is not None and hint_text(hint).startswith("You, ") and "Shout two" in hint_text(hint), f"annotation: {hint and hint_text(hint)}")
    check(hints(client, app_uri, 0, 1) == [] and len(hints(client, app_uri, 1, 50)) == 1, "chunk boundary: annotation returned once")
    resolved = client.request("inlayHint/resolve", hint) if hint else {"label": [{}]}
    tooltip = resolved["label"][0].get("tooltip", {}).get("value", "")
    check("[Open Changes with Previous Revision](revs://changes?id=" in tooltip, "hover card link")
    check(f"[Open Commit on GitHub](https://github.com/example/project/commit/{c[2]})" in tooltip, "GitHub link")
    check(set(hint["data"]) == {"id"} if hint else False, f"hint data is an id only: {hint and hint['data']}")
    if hint:
        forged = dict(hint, data={"uri": uri(crlf), "line": 1})
        resolved = client.request("inlayHint/resolve", forged)
        check("tooltip" not in json.dumps(resolved), "hints with made-up data get no card")
    cursor(client, app_uri, 2)
    hint = wait_for_hint(client, app_uri, 2)
    check(hint is not None and "Uncommitted changes" in hint_text(hint), "edited line shows uncommitted")
    resolved = client.request("inlayHint/resolve", hint) if hint else {"label": [{}]}
    unsaved_tooltip = resolved["label"][0].get("tooltip", {}).get("value", "")
    cursor(client, app_uri, 4)
    client.pump(0.2)
    check(hints(client, app_uri) == [], "no annotation on the empty line after the final newline")
    run(client, TOGGLE, wait=0.3)
    check(hints(client, app_uri) == [], "toggled off")

    print("a second Revs process gets the diff")
    client2 = Client()
    client2.request("initialize", {"processId": os.getpid(), "rootUri": uri(REPO), "capabilities": {}})
    client2.notify("initialized", {})
    cursor(client, view34_uri, 1)
    call, _ = run(client, PREVIOUS)  # this process runs the step...
    check(call is not None and diff_of(call)[1] == view12, "c1 ↔ c2 from c3 ↔ c4")
    open_doc(client2, view12)  # ...and Zed hands the new tab to the other one
    client2.pump(0.1)
    call, messages = run(client2, NEXT)  # no cursor report yet: the landing comes from the record
    check(call is not None and diff_of(call)[1] == view34 and diff_of(call)[2] == 2, f"the other process goes straight back: {call and diff_of(call)} {messages}")
    client.notify("textDocument/didChange", {  # two unsaved lines at the top of app.txt, in the first process
        "textDocument": {"uri": app_uri, "version": 20},
        "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "x\ny\n"}],
    })
    client.pump(0.1)
    cursor(client2, view12_uri, 1, character=1)  # TWO, in the second process's diff
    run(client2, OPEN_FILE)
    shown = client2.show_documents[-1] if client2.show_documents else {}
    check(shown.get("selection", {}).get("start", {}).get("line") == 3,
          f"the working file's unsaved lines come from the process that has it: {shown.get('selection')}")
    client.notify("textDocument/didChange", {
        "textDocument": {"uri": app_uri, "version": 21},
        "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}, "text": ""}],
    })

    print("cleanup keeps files other windows show")
    old = time.time() - 8 * 24 * 3600
    orphan = os.path.join(snapshots, "orphan.txt")
    open(orphan, "w").write("x\n")
    os.utime(orphan, (old, old))
    os.utime(view12, (old, old))
    old_layout = os.path.join(CACHE, ".dotfiles", "src", "1234567 a.txt")
    os.makedirs(os.path.dirname(old_layout), exist_ok=True)
    open(old_layout, "w").write("x\n")
    os.utime(old_layout, (old, old))
    client3 = subprocess.Popen([BIN, "lsp"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, env=env())
    time.sleep(0.5)
    client3.stdin.close()
    client3.wait(timeout=5)
    check(not os.path.exists(orphan), "a week-old snapshot nobody shows is removed")
    check(not os.path.exists(old_layout), "and so are week-old files of the earlier cache layout")
    check(os.path.exists(view12), "a week-old snapshot an open diff shows stays")
    client2.request("shutdown", None)
    client2.notify("exit", None)
    client2.process.wait(timeout=5)

    print("single-file project")
    client4 = Client()
    client4.request("initialize", {"processId": os.getpid(), "rootUri": uri(os.path.join(REPO, "src")),
        "workspaceFolders": [{"uri": uri(app), "name": "app.txt"}], "capabilities": {}})
    client4.notify("initialized", {})
    open_doc(client4, app, text="ONE\nTWO\nfour\nsix\n")
    cursor(client4, app_uri, 1)
    call, _ = run(client4, PREVIOUS)
    check(call is not None and call["args"][:2] == ["-e", app] and diff_of(call)[1] == view12, f"the file goes to Zed with the diff: {call and call['args'][:2]}")
    client4.request("shutdown", None)
    client4.notify("exit", None)
    client4.process.wait(timeout=5)

    print("working snapshots a diff shows are never rewritten")
    def working_snapshots():
        return {name for name in os.listdir(snapshots) if name.startswith("Working") and name.endswith(" app.txt")}
    shown = text_of(working_view)
    for name in working_snapshots():  # all old; only the one this window's diff shows is leased
        os.utime(os.path.join(snapshots, name), (time.time() - 120,) * 2)
    open(app, "w").write("ONE\nTWO\nfour\nsix\nseven\n")
    subprocess.run([BIN, "previous", app, "5"], env=env(), check=True, cwd=REPO)
    written = diff_of(cli_calls()[-1])[1]
    check(written != working_view and os.path.basename(written).startswith("Working"), f"another working version goes elsewhere: {os.path.basename(written)}")
    check(text_of(working_view) == shown, "the shown one is left alone")
    before = working_snapshots()
    os.utime(written, (time.time() - 120,) * 2)  # a while ago, and no diff shows it
    open(app, "w").write("ONE\nTWO\nfour\nsix\neight\n")
    subprocess.run([BIN, "previous", app, "5"], env=env(), check=True, cwd=REPO)
    reused = diff_of(cli_calls()[-1])[1]
    check(working_snapshots() == before and reused != working_view and "eight" in text_of(reused), "an unused older working snapshot is reused")
    git("checkout", "--", "src/app.txt")

    print("shutdown")
    rss = subprocess.run(["ps", "-o", "rss=", "-p", str(client.process.pid)], capture_output=True, text=True).stdout.strip()
    print(f"  server RSS: {int(rss) / 1024:.1f} MB")
    client.request("shutdown", None)
    client.notify("exit", None)
    check(client.process.wait(timeout=5) == 0, "clean exit")

    print("URL handler and command line")
    url = tooltip.split("](revs://", 1)[1].split(")", 1)[0] if "](revs://" in tooltip else ""
    check(url.startswith("changes?id=") and len(url) == len("changes?id=") + 32, f"links carry only a random id: {url}")
    links = os.path.join(CACHE, ".links")
    check(oct(os.stat(links).st_mode & 0o777) == "0o700", "link records are private")
    unknown = "changes?id=" + "0" * 32
    check(subprocess.run([BIN, "url", "revs://" + unknown], env=env(), capture_output=True).returncode != 0, "unknown links are refused")
    target = os.path.join(WORK, "pwned")
    planted = "1" * 32
    json.dump({"repo": REPO, "root": REPO, "file": "src/app.txt", "sha": "--output=" + target, "line": 0}, open(os.path.join(links, planted), "w"))
    result = subprocess.run([BIN, "url", "revs://changes?id=" + planted], env=env(), capture_output=True)
    check(result.returncode != 0 and not os.path.exists(target), "option-like revisions are rejected")
    before = len(cli_calls())
    subprocess.run([BIN, "url", "revs://" + url], env=env(), check=True)
    calls = cli_calls()
    check(len(calls) == before + 1 and diff_of(calls[-1])[1] == view12, "revs:// link opens the line's change")
    link_file = os.path.join(links, url.split("id=", 1)[1])
    os.utime(link_file, (time.time() - 2 * 24 * 3600,) * 2)
    check(subprocess.run([BIN, "url", "revs://" + url], env=env(), capture_output=True).returncode != 0, "day-old links are refused")
    unsaved = unsaved_tooltip.split("](revs://", 1)[1].split(")", 1)[0] if "](revs://" in unsaved_tooltip else ""
    result = subprocess.run([BIN, "url", "revs://" + unsaved], env=env(), capture_output=True, text=True)
    check(unsaved and result.returncode != 0 and "save the file first" in result.stderr, f"an unsaved line's link asks to save: {result.stderr.strip()}")
    subprocess.run([BIN, "previous", app, "2"], env=env(), check=True, cwd=REPO)
    check(diff_of(cli_calls()[-1])[1] == right_file("old_name.txt", short[2]), "command-line previous")
    subprocess.run([BIN, "next", view34, "1"], env=env(), check=True, cwd=REPO)
    check(diff_of(cli_calls()[-1])[1] == right_file("app.txt", short[5]), "command-line next from a snapshot")


    print()
    if failures:
        print(f"{len(failures)} FAILED")
        sys.exit(1)
    print("all passed")


if __name__ == "__main__":
    main()
