import base64
import http.client
import json
import os
import shutil
import time
import struct
import sys

AUTH = "Basic " + base64.b64encode(b"user:").decode()
IGNORED = {"date", "content-length"}


def request(port, method, path, body=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    connection.request(method, path, body=body, headers={"Authorization": AUTH, **(headers or {})})
    response = connection.getresponse()
    data = response.read()
    connection.close()
    kept = sorted((k.lower(), v) for k, v in response.getheaders() if k.lower() not in IGNORED)
    return response.status, kept, decoded(response.status, kept, data)


def decoded(status, headers, data):
    json_answer = ("content-type", "application/json") in headers and ("content-encoding", "gzip") not in headers
    if status != 200 or not json_answer:
        return data
    try:
        return json.loads(data)
    except ValueError:
        return data


def unary(port, rpc, message, headers=None):
    return request(port, "POST", "/" + rpc, json.dumps(message), {"Content-Type": "application/json", **(headers or {})})


def envelope(message):
    payload = json.dumps(message).encode()
    return struct.pack(">BI", 0, len(payload)) + payload


def stream(port, rpc, message, headers=None):
    status, kept, data = request(
        port, "POST", "/" + rpc, envelope(message), {"Content-Type": "application/connect+json", **(headers or {})}
    )
    return status, kept, frames(data)


def frames(data):
    found = []
    while len(data) >= 5:
        flags, size = struct.unpack(">BI", data[:5])
        payload = json.loads(data[5 : 5 + size])
        payload.get("event", {}).get("start", {}).pop("pid", None)
        found.append((flags, payload))
        data = data[5 + size :]
    return found


def start_sleep(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    connection.request(
        "POST",
        "/process.Process/Start",
        body=envelope({"process": {"cmd": "/bin/sleep", "args": ["30"]}}),
        headers={"Authorization": AUTH, "Content-Type": "application/connect+json"},
    )
    response = connection.getresponse()
    flags, size = struct.unpack(">BI", response.read(5))
    event = json.loads(response.read(size))
    return connection, event["event"]["start"]["pid"]


def signal_cases(port):
    connection, pid = start_sleep(port)
    results = {
        f"SendSignal {name}": unary(port, "process.Process/SendSignal", {"process": {"pid": pid}, "signal": value})
        for name, value in [("unspecified", "SIGNAL_UNSPECIFIED"), ("number 7", 7), ("number 0", 0)]
    }
    unary(port, "process.Process/SendSignal", {"process": {"pid": pid}, "signal": "SIGNAL_SIGKILL"})
    connection.close()
    return results


def cases(port):
    stat = {"path": "/"}
    results = {
        "GET /health": request(port, "GET", "/health"),
        "GET /envs": request(port, "GET", "/envs"),
        "GET /files missing": request(port, "GET", "/files?path=/nonexistent"),
        "GET /files dir": request(port, "GET", "/files?path=/"),
        "POST /fsfreeze bad": request(port, "POST", "/fsfreeze"),
    }
    for timeout in ["abc", "-1", "0", "12345678901", "", "1.5"]:
        results[f"Stat Connect-Timeout-Ms={timeout!r}"] = unary(
            port, "filesystem.Filesystem/Stat", stat, {"Connect-Timeout-Ms": timeout}
        )
    true = {"process": {"cmd": "/bin/true"}}
    for timeout in ["abc", "0", "-5"]:
        results[f"Start Connect-Timeout-Ms={timeout!r}"] = stream(
            port, "process.Process/Start", true, {"Connect-Timeout-Ms": timeout}
        )
    gzip = {"Accept-Encoding": "gzip"}
    results["Stat gzip"] = unary(port, "filesystem.Filesystem/Stat", stat, gzip)[:2]
    results["ListDir gzip"] = unary(port, "filesystem.Filesystem/ListDir", {"path": "/usr/bin"}, gzip)[:2]
    results["Stat missing gzip"] = unary(port, "filesystem.Filesystem/Stat", {"path": "/nope"}, gzip)
    results.update(signal_cases(port))
    results.update(filesystem_cases(port))
    results["POST /init"] = request(port, "POST", "/init", "{}", {"Content-Type": "application/json"})
    return results


def fixture(port):
    base = f"/tmp/fsdiff-{port}/root"
    shutil.rmtree(os.path.dirname(base), ignore_errors=True)
    os.makedirs(f"{base}/sub/deeper")
    os.makedirs(f"{base}/empty")
    for path, text in [("a.txt", "hello"), ("sub/b.txt", "bee"), ("sub/deeper/c.txt", "")]:
        with open(f"{base}/{path}", "w") as file:
            file.write(text)
    os.symlink(f"{base}/a.txt", f"{base}/link")
    os.symlink(f"{base}/nowhere", f"{base}/dangling")
    os.chmod(f"{base}/a.txt", 0o640)
    shutil.chown(base, "user", "user")
    for root, dirs, files in os.walk(base):
        for name in dirs + files:
            os.lchown(os.path.join(root, name), 1000, 1000)
    return base


def scrubbed(value):
    if isinstance(value, dict):
        return {key: scrubbed(item) for key, item in value.items() if key != "modifiedTime"}
    if isinstance(value, list):
        return [scrubbed(item) for item in value]
    return value


def fs(port, base, rpc, message):
    status, headers, body = unary(port, "filesystem.Filesystem/" + rpc, message)
    if isinstance(body, bytes):
        return status, headers, body.replace(base.encode(), b"BASE")
    return status, headers, scrubbed(json.loads(json.dumps(body).replace(base, "BASE")))


def filesystem_cases(port):
    base = fixture(port)
    cases = {}
    for name in ["a.txt", "link", "dangling", "sub", "missing", ""]:
        cases[f"Stat {name!r}"] = fs(port, base, "Stat", {"path": f"{base}/{name}"})
    for path, depth in [("", 1), ("", 2), ("", 0), ("sub", 5), ("a.txt", 1), ("missing", 1), ("link", 1)]:
        cases[f"ListDir {path!r} depth {depth}"] = fs(port, base, "ListDir", {"path": f"{base}/{path}", "depth": depth})
    for path in ["new", "new", "a.txt/x", "deep/er/dir"]:
        cases[f"MakeDir {path!r}"] = fs(port, base, "MakeDir", {"path": f"{base}/{path}"})
    for source, destination in [("a.txt", "moved.txt"), ("missing", "x"), ("moved.txt", "sub"), ("empty", "sub/deeper")]:
        cases[f"Move {source!r} {destination!r}"] = fs(
            port, base, "Move", {"source": f"{base}/{source}", "destination": f"{base}/{destination}"}
        )
    for path in ["moved.txt", "missing", "sub", "dangling"]:
        cases[f"Remove {path!r}"] = fs(port, base, "Remove", {"path": f"{base}/{path}"})
    cases["Stat relative"] = fs(port, base, "Stat", {"path": "relative/path"})
    created = unary(port, "filesystem.Filesystem/CreateWatcher", {"path": base})
    watcher = created[2].get("watcherId", "") if isinstance(created[2], dict) else ""
    cases["CreateWatcher"] = (created[0], created[1], bool(watcher))
    with open(f"{base}/watched.txt", "w") as file:
        file.write("x")
    time.sleep(0.5)
    events = fs(port, base, "GetWatcherEvents", {"watcherId": watcher})
    cases["GetWatcherEvents"] = events
    cases["RemoveWatcher"] = unary(port, "filesystem.Filesystem/RemoveWatcher", {"watcherId": watcher})
    cases["RemoveWatcher unknown"] = unary(port, "filesystem.Filesystem/RemoveWatcher", {"watcherId": "nope"})
    cases["GetWatcherEvents unknown"] = unary(port, "filesystem.Filesystem/GetWatcherEvents", {"watcherId": "nope"})
    cases["CreateWatcher missing"] = fs(port, base, "CreateWatcher", {"path": f"{base}/missing"})
    return cases


def metrics(port):
    status, _, body = request(port, "GET", "/metrics")
    return status, body


def main():
    go_port, rust_port = int(sys.argv[1]), int(sys.argv[2])
    go, rust = cases(go_port), cases(rust_port)
    differences = 0
    for name in go:
        if go[name] == rust[name]:
            print(f"same  {name}")
            continue
        differences += 1
        print(f"DIFF  {name}")
        print(f"  go:   {go[name]}")
        print(f"  rust: {rust[name]}")
    go_metrics, rust_metrics = metrics(go_port), metrics(rust_port)
    print(f"metrics go:   {go_metrics}")
    print(f"metrics rust: {rust_metrics}")
    print(f"{differences} differences")


main()
