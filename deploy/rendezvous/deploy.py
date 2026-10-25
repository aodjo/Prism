#!/usr/bin/env python3
"""Deploy a pinned Osaka image through a restricted SSH key, rolling back unhealthy starts."""

import fcntl
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path("/opt/prism")
IMAGE = re.compile(r"ghcr\.io/aodjo/prism-rendezvous:[0-9a-f]{40}@sha256:[0-9a-f]{64}")


def run(arguments, *, env=None, text=None):
    """Run one command without exposing registry credentials or expanded service secrets."""
    result = subprocess.run(arguments, env=env, input=text, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"{arguments[0]} {arguments[1]} failed ({result.returncode})")
    return result.stdout.strip()


def validate_service(config, image):
    """Reject requests that affect another stack or expand the container's host privileges."""
    if config.get("name") != "prism-rendezvous" or set(config.get("services", {})) != {"rendezvous"}:
        raise ValueError("only the Prism rendezvous stack can be deployed")
    service = config["services"]["rendezvous"]
    if (
        service.get("image") != image
        or service.get("container_name") != "prism-rendezvous"
        or service.get("network_mode") != "host"
        or service.get("privileged", False)
        or service.get("volumes")
        or service.get("devices")
        or service.get("cap_add")
        or not service.get("read_only")
        or "ALL" not in service.get("cap_drop", [])
        or not any(value in {"no-new-privileges", "no-new-privileges:true"}
                   for value in service.get("security_opt", []))
    ):
        raise ValueError("the rendezvous service must keep its restricted container settings")
    command = service.get("command", [])
    bind = command[command.index("--bind") + 1]
    region = command[command.index("--region") + 1]
    if bind != "0.0.0.0:47300" or region != "Japan (Osaka)":
        raise ValueError("this deploy key is restricted to the Osaka region")


def healthy():
    """Check the running container, both UDP ports, and an actual rendezvous response."""
    try:
        if run(["docker", "inspect", "-f", "{{.State.Running}}", "prism-rendezvous"]) != "true":
            return False
        ports = run(["ss", "-H", "-lnu"])
        if not all(re.search(rf":{port}\s", ports) for port in (47300, 47301)):
            return False
        nonce = os.urandom(8)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as peer:
            peer.settimeout(1)
            peer.sendto(b"\x0c" + nonce, ("127.0.0.1", 47300))
            answer, _ = peer.recvfrom(256)
        return answer[:9] == b"\x0d" + nonce and answer[9:].split(b"\0")[0] == b"Japan (Osaka)"
    except (OSError, RuntimeError):
        return False


def activate(compose, candidate, previous, env, *, check=healthy, pause=time.sleep):
    """Apply a candidate and restore the previous image and configuration if it fails health checks."""
    original = compose.read_text()
    command = ["docker", "compose", "-f", str(compose), "--env-file", str(ROOT / ".env")]
    compose.write_text(candidate)
    try:
        run(command + ["up", "-d", "--no-build", "--pull", "never"], env=env)
        for _ in range(20):
            if check():
                return
            pause(1)
        raise RuntimeError("the new Osaka container did not pass its health checks")
    except Exception:
        compose.write_text(original)
        run(command + ["up", "-d", "--no-build", "--pull", "never"],
            env={**env, "PRISM_IMAGE": previous})
        raise


def main():
    """Read a bounded deploy request on stdin; no arbitrary SSH commands are accepted."""
    request = json.loads(sys.stdin.read(1024 * 1024))
    image = request.get("image", "")
    candidate = request.get("compose", "")
    if not isinstance(image, str) or not IMAGE.fullmatch(image):
        raise ValueError("a repository image pinned by commit and digest is required")
    if not isinstance(candidate, str) or not 0 < len(candidate) < 65536:
        raise ValueError("a bounded compose configuration is required")

    with (ROOT / ".deploy.lock").open("w") as lock, tempfile.TemporaryDirectory() as scratch:
        fcntl.flock(lock, fcntl.LOCK_EX)
        env = {**os.environ, "DOCKER_CONFIG": scratch, "PRISM_IMAGE": image}
        compose = ROOT / "compose.yaml"
        previous = run(["docker", "inspect", "-f", "{{.Config.Image}}", "prism-rendezvous"])
        draft = Path(scratch) / "compose.yaml"
        draft.write_text(candidate)
        config = json.loads(run(["docker", "compose", "-f", str(draft), "--env-file",
                                 str(ROOT / ".env"), "config", "--format", "json"], env=env))
        validate_service(config, image)
        run(["docker", "login", "ghcr.io", "-u", request["registry_user"], "--password-stdin"],
            env=env, text=request["registry_token"])
        print("Pulling the pinned Osaka image", flush=True)
        run(["docker", "pull", image], env=env)
        architecture = run(["docker", "image", "inspect", "-f", "{{.Architecture}}", image], env=env)
        if architecture != "arm64":
            raise ValueError("the Osaka image must be ARM64")
        (ROOT / "compose.yaml.previous").write_text(compose.read_text())
        (ROOT / ".image.previous").write_text(previous + "\n")
        activate(compose, candidate, previous, env)
        (ROOT / ".image").write_text(image + "\n")
        print(json.dumps({"region": "Japan (Osaka)", "image": image, "architecture": architecture,
                          "healthy": True}))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"Deployment failed: {error}", file=sys.stderr)
        sys.exit(1)
