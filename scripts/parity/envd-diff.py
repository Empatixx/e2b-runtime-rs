import base64
import http.client
import json
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
    return response.status, kept, data


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
    results["POST /init"] = request(port, "POST", "/init", "{}", {"Content-Type": "application/json"})
    return results


def metrics(port):
    status, _, body = request(port, "GET", "/metrics")
    return status, json.loads(body) if status == 200 else body


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
