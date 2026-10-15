"""Recovery and stale-writer contracts with no external network calls."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location('remote_state', Path(__file__).with_name('remote_state.py'))
state = importlib.util.module_from_spec(spec)
spec.loader.exec_module(state)


def remote(value=None, sha='blob-sha'):
    value = {'cursor': 1} if value is None else value
    return {'content': base64.b64encode(json.dumps(value).encode()).decode(), 'sha': sha}


class RemoteStateTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / 'state.json'
        env = patch.dict(os.environ, {'DELIVERY_STATE_KEY': 'bot.json',
            'GITHUB_REPOSITORY': 'test/repo', 'GH_TOKEN': 'TEST'})
        env.start()
        self.addCleanup(env.stop)

    def operation(self, operation):
        with patch.object(sys, 'argv', ['remote_state.py', operation, str(self.path)]):
            state.main()

    def test_only_confirmed_absence_permits_migration(self):
        with patch.object(state, 'api', side_effect=[None, None, {'default_branch': 'main'}]):
            with self.assertRaises(SystemExit) as result:
                self.operation('restore')
            self.assertEqual(result.exception.code, 3)
        self.assertEqual(state.receipt(self.path).read_text(), state.BOOTSTRAP)
        for responses in ([None, {'object': {'sha': 'existing'}}], [None, None, None]):
            with patch.object(state, 'api', side_effect=responses):
                with self.assertRaises(RuntimeError): self.operation('restore')

    def test_network_auth_and_corruption_fail_closed(self):
        for error in (urllib.error.URLError('offline'),
                      urllib.error.HTTPError('https://invalid', 403, 'denied', {}, None),
                      TimeoutError()):
            with patch.object(state.urllib.request, 'urlopen', side_effect=error):
                with self.assertRaises(RuntimeError): self.operation('restore')
        for value in ([], 'not state'):
            with patch.object(state, 'api', return_value=remote(value)):
                with self.assertRaises(RuntimeError): self.operation('restore')
        self.assertFalse(self.path.exists())

    def test_stale_writer_and_missing_receipt_cannot_overwrite(self):
        with patch.object(state, 'api', return_value=remote()): self.operation('restore')
        for value in (None, remote({'newer': True}, 'new-sha')):
            with patch.object(state, 'api', return_value=value) as api:
                with self.assertRaisesRegex(RuntimeError, 'changed since restore'): self.operation('save')
                self.assertEqual(api.call_count, 1)
        state.receipt(self.path).unlink()
        with patch.object(state, 'api', return_value=remote()):
            with self.assertRaises(FileNotFoundError): self.operation('save')

    def test_intent_and_ack_use_previous_blob_and_update_receipt(self):
        with patch.object(state, 'api', return_value=remote()): self.operation('restore')
        self.path.write_text(json.dumps({'awaiting_ack': True}))
        with patch.object(state, 'api', side_effect=[remote(), {'content': {'sha': 'intent'}}]) as api:
            self.operation('save')
            payload = api.call_args.args[2]
            self.assertEqual(payload['sha'], 'blob-sha')
            self.assertTrue(json.loads(base64.b64decode(payload['content']))['awaiting_ack'])
        self.assertEqual(state.receipt(self.path).read_text(), 'intent')
        intent = remote({'awaiting_ack': True}, 'intent')
        self.path.write_text(json.dumps({'cursor': 2, 'awaiting_ack': False}))
        with patch.object(state, 'api', side_effect=[intent, RuntimeError('conflict')]):
            with self.assertRaises(RuntimeError): self.operation('save')
        self.path.unlink()
        with patch.object(state, 'api', return_value=intent): self.operation('restore')
        self.assertTrue(json.loads(self.path.read_text())['awaiting_ack'])

    def test_unchanged_state_skips_write(self):
        with patch.object(state, 'api', return_value=remote()) as api:
            self.operation('restore')
            api.reset_mock()
            self.operation('save')
            self.assertEqual(api.call_count, 1)

    def test_large_state_uses_blob_api_and_keeps_contents_sha(self):
        with patch.object(state, 'api', side_effect=[{'sha': 'large', 'encoding': 'none'}, remote()]) as api:
            self.operation('restore')
            self.assertEqual(api.call_args.args, ('/git/blobs/large',))
            self.assertEqual(state.receipt(self.path).read_text(), 'large')


if __name__ == '__main__': unittest.main()
