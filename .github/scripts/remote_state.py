"""CAS checkpoints independent of the final, best-effort source archive push."""
import base64
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request

BRANCH = 'delivery-state'
BOOTSTRAP = 'confirmed-absent'


def receipt(path):
    return Path(str(path) + '.remote-sha')


def remember(path, sha):
    if not isinstance(sha, str) or not sha:
        raise RuntimeError('Checkpoint response omitted the committed blob')
    destination = receipt(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(str(destination) + '.tmp')
    temporary.write_text(sha)
    temporary.replace(destination)


def api(endpoint, method='GET', payload=None):
    request = urllib.request.Request(
        'https://api.github.com/repos/' + os.environ['GITHUB_REPOSITORY'] + endpoint,
        data=None if payload is None else json.dumps(payload).encode(), method=method,
        headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'],
                 'Accept': 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28'},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404 and method == 'GET':
            return None
        raise RuntimeError(f'Checkpoint API failed: HTTP {error.code}') from None
    except (urllib.error.URLError, TimeoutError):
        raise RuntimeError('Checkpoint API connection failed') from None


def decode(remote):
    # Contents responses omit content beyond 1 MiB; the blob API supports it.
    if remote.get('encoding') == 'none':
        remote = api('/git/blobs/' + remote['sha'])
    content = base64.b64decode(''.join(remote['content'].split()), validate=True)
    if not isinstance(json.loads(content), dict):
        raise RuntimeError('Invalid checkpoint state')
    return content


def main():
    operation, local_file = sys.argv[1:]
    path = Path(local_file)
    key = os.environ['DELIVERY_STATE_KEY']
    if '/' in key or key in ('', '.', '..'):
        raise RuntimeError('Checkpoint key must be a filename')
    endpoint = '/contents/' + urllib.parse.quote(key, safe='')
    remote = api(endpoint + '?ref=' + BRANCH)
    content = None if remote is None else decode(remote)
    if operation == 'restore':
        if remote is None:
            if api('/git/ref/heads/' + BRANCH) is not None:
                raise RuntimeError('Existing checkpoint branch has no state')
            if api('') is None:
                raise RuntimeError('Repository unavailable; cannot establish initial state')
            remember(path, BOOTSTRAP)
            sys.exit(3)
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = Path(str(path) + '.tmp')
        temporary.write_bytes(content)
        temporary.replace(path)
        remember(path, remote['sha'])
        return
    if operation != 'save':
        raise RuntimeError('Unknown checkpoint operation')
    expected = receipt(path).read_text()
    if (remote is None and expected != BOOTSTRAP) or (remote is not None and remote['sha'] != expected):
        raise RuntimeError('Checkpoint changed since restore; refusing stale state')
    local = path.read_bytes()
    if not isinstance(json.loads(local), dict):
        raise RuntimeError('Invalid local checkpoint state')
    if remote is None:
        if api('/git/ref/heads/' + BRANCH) is not None:
            raise RuntimeError('Existing checkpoint branch has no state')
        repository = api('')
        head = api('/git/ref/heads/' + repository['default_branch'])
        api('/git/refs', 'POST', {'ref': 'refs/heads/' + BRANCH, 'sha': head['object']['sha']})
    if local == content:
        return
    payload = {'message': 'Checkpoint delivery progress', 'branch': BRANCH,
               'content': base64.b64encode(local).decode()}
    if remote is not None:
        payload['sha'] = remote['sha']
    response = api(endpoint, 'PUT', payload)
    remember(path, response.get('content', {}).get('sha'))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Delivery checkpoint failed: {error}', file=sys.stderr)
        sys.exit(1)
