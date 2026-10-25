"""Exercise deployment isolation and recovery without touching Docker or the host network."""

import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import deploy


class DeploymentTests(unittest.TestCase):
    """Keep failed and out-of-scope deployment requests from disrupting the running stack."""

    def service(self):
        """Return the resolved, restricted Osaka service accepted by the deploy helper."""
        return {
            "name": "prism-rendezvous",
            "services": {"rendezvous": {
                "image": "candidate", "container_name": "prism-rendezvous", "network_mode": "host",
                "read_only": True, "cap_drop": ["ALL"], "security_opt": ["no-new-privileges:true"],
                "command": ["--bind", "0.0.0.0:47300", "--region", "Japan (Osaka)"],
            }},
        }

    def test_only_the_existing_osaka_stack_can_be_changed(self):
        """Reject another service, host mounts, additional privileges, and another region."""
        config = self.service()
        deploy.validate_service(config, "candidate")
        for change in ({"privileged": True}, {"volumes": ["/:/host"]}, {"cap_add": ["SYS_ADMIN"]},
                       {"command": ["--bind", "0.0.0.0:47300", "--region", "Another region"]}):
            changed = copy.deepcopy(config)
            changed["services"]["rendezvous"].update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                deploy.validate_service(changed, "candidate")
        config["services"]["unrelated"] = {}
        with self.assertRaises(ValueError):
            deploy.validate_service(config, "candidate")

    def test_an_unhealthy_image_restores_the_previous_image_and_config(self):
        """A container that starts but never answers must roll back instead of staying deployed."""
        with tempfile.TemporaryDirectory() as folder, patch.object(deploy, "run") as command:
            compose = Path(folder) / "compose.yaml"
            compose.write_text("previous config")
            with self.assertRaisesRegex(RuntimeError, "health checks"):
                deploy.activate(compose, "candidate config", "previous image", {"PRISM_IMAGE": "candidate"},
                                check=lambda: False, pause=lambda _: None)
            self.assertEqual(compose.read_text(), "previous config")
            self.assertEqual(command.call_count, 2)
            self.assertEqual(command.call_args.kwargs["env"]["PRISM_IMAGE"], "previous image")

    def test_a_failed_container_start_also_rolls_back(self):
        """Recover the prior configuration when Compose fails before health checking starts."""
        with tempfile.TemporaryDirectory() as folder, patch.object(deploy, "run") as command:
            command.side_effect = [RuntimeError("start failed"), ""]
            compose = Path(folder) / "compose.yaml"
            compose.write_text("previous config")
            with self.assertRaisesRegex(RuntimeError, "start failed"):
                deploy.activate(compose, "candidate config", "previous image", {}, check=lambda: True)
            self.assertEqual(compose.read_text(), "previous config")
            self.assertEqual(command.call_count, 2)

    def test_a_healthy_candidate_keeps_the_new_configuration(self):
        """Successful deployment leaves the candidate in place and never starts the old image."""
        with tempfile.TemporaryDirectory() as folder, patch.object(deploy, "run") as command:
            compose = Path(folder) / "compose.yaml"
            compose.write_text("previous config")
            deploy.activate(compose, "candidate config", "previous image", {}, check=lambda: True)
            self.assertEqual(compose.read_text(), "candidate config")
            self.assertEqual(command.call_count, 1)


if __name__ == "__main__":
    unittest.main()
