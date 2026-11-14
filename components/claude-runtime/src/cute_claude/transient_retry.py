"""Bounded silent-turn fallback; never an HTTP retry or a partial-turn replay."""
import random


RETRY_CONTINUATION = ('The previous request failed before any response arrived; '
                      'please answer the message above.')


def retry_delay(error, attempt: int) -> float | None:
    # No imports from subscription here: the classifier is also used by its driver.
    if (attempt >= 2 or getattr(error, 'transient_refresh', False)
            or getattr(error, 'native_code', None) not in {'server_error', 'unknown'}):
        return None
    diagnostic = getattr(error, 'diagnostic', None)
    if diagnostic is None:
        return None
    status = diagnostic.http_status
    if status is not None:
        if not 500 <= status <= 599:
            return None
    elif diagnostic.category not in {'ECONNRESET', 'ENOTFOUND', 'ETIMEDOUT', 'overloaded'}:
        return None
    # The SDK normally drops HTTP headers. A missing Retry-After is not zero:
    # use a one-second floor, exponential growth, and positive jitter.
    base = float(2 ** attempt)
    retry_after = diagnostic.retry_after
    if retry_after is not None and retry_after > 60:
        return None  # Do not retry earlier than the server asked or wait unboundedly.
    return max(base + random.uniform(0, base * 0.25), retry_after or 0)
