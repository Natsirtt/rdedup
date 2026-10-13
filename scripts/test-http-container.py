#!/usr/bin/env python3
"""Exercise the built Linux image using a loopback-only port and disposable repository."""
import argparse
import contextlib
import hashlib
import json
import os
import random
import secrets
import subprocess
import tempfile
import time
import uuid
import urllib.error
import urllib.request
from pathlib import Path


def docker(*arguments, input=None, check=True, environment=None):
    return subprocess.run(
        ["docker", *arguments], input=input, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, check=check, env=environment, timeout=180,
    )


@contextlib.contextmanager
def running_server(image):
    name = "rdedup-http-test-" + uuid.uuid4().hex
    volume = name + "-repository"
    token = secrets.token_hex(32)
    with tempfile.TemporaryDirectory(prefix="rdedup-container-") as directory:
        configuration = Path(directory) / "server.toml"
        configuration.write_text(
            'require_read_credentials = true\n[[credentials]]\n'
            'name = "acceptance-test"\ntoken = "' + token + '"\n',
            encoding="utf-8",
        )
        docker("volume", "create", volume)
        try:
            docker(
                "run", "--detach", "--name", name, "--read-only",
                "--tmpfs", "/tmp:rw,nosuid,nodev", "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges", "--publish", "127.0.0.1::8080",
                "--mount", f"type=volume,source={volume},target=/repository",
                "--mount", f"type=bind,source={configuration},target=/etc/rdedup.toml,readonly",
                image, "--config", "/etc/rdedup.toml",
            )
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                logs = docker("logs", name)
                if b"Serving plain HTTP on" in logs.stderr:
                    break
                state = json.loads(docker("inspect", name).stdout)[0]["State"]
                if not state["Running"]:
                    raise AssertionError(logs.stderr.decode())
                time.sleep(0.1)
            else:
                raise AssertionError("container did not become ready")
            yield name, token
        finally:
            docker("rm", "--force", name, check=False)
            docker("volume", "rm", volume, check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="rdedup-http:validation")
    image = parser.parse_args().image
    with running_server(image) as (name, token):
        environment = dict(os.environ, RDEDUP_HTTP_TOKEN=token)

        def client(*arguments, input=None, authenticated=True, check=True):
            credential = ["--env", "RDEDUP_HTTP_TOKEN"] if authenticated else []
            return docker(
                "exec", "--interactive", *credential, name, "rdedup",
                "--repo", "http://127.0.0.1:8080", *arguments,
                input=input, check=check, environment=environment,
            )

        def protocol(method, path, payload=None):
            address = docker("port", name, "8080/tcp").stdout.decode().strip()
            request = urllib.request.Request(
                "http://" + address + path, method=method,
                data=json.dumps(payload).encode() if payload is not None else None,
                headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
            )
            try:
                response = urllib.request.urlopen(request, timeout=10)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                return response.status, json.load(response)

        identity = docker("exec", name, "id", "-u").stdout.strip()
        assert identity == b"10001", identity
        client("init", "--encryption", "none")
        contents = random.Random(57).randbytes(8 * 1024 * 1024 + 123)
        client("write", "artifact", "--spool", input=contents)
        restored = client("load", "artifact").stdout
        assert hashlib.sha256(restored).digest() == hashlib.sha256(contents).digest()
        client("write", "artifact", "--spool", input=contents)
        conflict = client("write", "artifact", input=b"different", check=False)
        assert conflict.returncode != 0, "different contents replaced a name"
        anonymous = client("list", authenticated=False, check=False)
        assert anonymous.returncode != 0, "private reads accepted no credential"
        client("gc")
        assert client("load", "artifact").stdout == contents
        request_path = "/api/v1/lease-requests/" + str(uuid.uuid4())
        status, acquisition = protocol("PUT", request_path, {"mode": "shared"})
        assert status == 200, acquisition
        deadline = time.monotonic() + 10
        while acquisition["state"] == "queued" and time.monotonic() < deadline:
            time.sleep(0.05)
            status, acquisition = protocol("GET", request_path)
            assert status == 200, acquisition
        assert acquisition["state"] == "granted", acquisition
        lease = acquisition["lease"]["id"]
        docker("stop", "--time", "15", name)
        state = json.loads(docker("inspect", name).stdout)[0]["State"]
        assert state["ExitCode"] == 0, state
        docker("start", name)
        # Acquisition retries cover the brief interval before the listener binds.
        assert client("load", "artifact").stdout == contents
        status, problem = protocol("PATCH", "/api/v1/leases/" + lease)
        assert status == 410 and problem["type"] == "urn:rdedup:problem:lease-expired", problem
        client("rm", "artifact")
        client("gc")
        assert client("list").stdout.strip() == b""
        docker("stop", "--time", "15", name)
        state = json.loads(docker("inspect", name).stdout)[0]["State"]
        assert state["ExitCode"] == 0, state
    print("Container publication, name retries, private reads, GC, persistence, restart lease loss and SIGTERM passed.")


if __name__ == "__main__":
    main()
