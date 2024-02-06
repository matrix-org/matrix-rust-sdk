"""
A mitmproxy script that introduces certain request failures in a deterministic
way.

Used mainly for Matrix style requests.

To run execute it with mitmproxy:

    >>> mitmproxy -s failures.py`

"""
import time
import json
import random

from mitmproxy import http
from mitmproxy.script import concurrent

REQUEST_COUNT = 0


def timeout(flow):
    timeout = 60 if "sync" in flow.request.pretty_url else 30
    time.sleep(timeout)
    return None


# A map holding our failure modes.
# The keys are just descriptive names for the failure mode while the values
# hold a tuple containing a function that may or may not create a failure and
# the probability weight at which rate this failure should be triggered.
#
# The method should return an http.Response if it should modify the
# response or None if the response should be passed as is.
FAILURES = {
    "Success": (lambda x: None, 50),
    "Gateway error":
    (lambda _: http.Response.make(500, b"Gateway error"), 20),
    "Limit exceeded": (lambda _: http.Response.make(
        429,
        json.dumps({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Too many requests",
            "retry_after_ms": 2000
        })), 20),
    "Timeout error": (timeout, 10)
}


@concurrent
def request(flow):
    global FAILURES

    weights = [weight for (_, weight) in FAILURES.values()]
    failure = random.choices(list(FAILURES), weights=weights)[0]
    failure_func, _ = FAILURES[failure]

    response = failure_func(flow)

    if response:
        flow.response = response
