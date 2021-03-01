"""
A mitmproxy script that blocks and removes well known Matrix server
information.

There are two ways a Matrix server can trigger the client to reconfigure the
homeserver URL:

    1. By responding to a `./well-known/matrix/client` request with a new
    homeserver URL.

    2. By including a new homeserver URL inside the `/login` response.

To run execute it with mitmproxy:

    >>> mitmproxy -s well-known-block.py`

"""
import json

from mitmproxy import http


def request(flow):
    if flow.request.path == "/.well-known/matrix/client":
        flow.response = http.HTTPResponse.make(
            404,  # (optional) status code
            b"Not found",  # (optional) content
            {"Content-Type": "text/html"}  # (optional) headers
        )


def response(flow: http.HTTPFlow):
    if flow.request.path == "/_matrix/client/r0/login":
        if flow.response.status_code == 200:
            body = json.loads(flow.response.content)
            body.pop("well_known", None)
            flow.response.text = json.dumps(body)
