"""
A mitmproxy script that introduces certain request failures in a deterministic
way.

Used mainly for Matrix style requests.

To run execute it with mitmproxy:

    >>> mitmproxy -s failures.py`

"""
import time
import json

from mitmproxy import http
from mitmproxy.script import concurrent

REQUEST_COUNT = 0


@concurrent
def request(flow):
    global REQUEST_COUNT

    REQUEST_COUNT += 1

    if REQUEST_COUNT % 2 == 0:
        return
    elif REQUEST_COUNT % 3 == 0:
        flow.response = http.HTTPResponse.make(
            500,
            b"Gateway error",
        )
    elif REQUEST_COUNT % 7 == 0:
        if "sync" in flow.request.pretty_url:
            time.sleep(60)
        else:
            time.sleep(30)
    else:
        flow.response = http.HTTPResponse.make(
            429,
            json.dumps({
                "errcode": "M_LIMIT_EXCEEDED",
                "error": "Too many requests",
                "retry_after_ms": 2000
            })
        )
