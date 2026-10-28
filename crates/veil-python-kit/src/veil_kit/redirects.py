"""Redirects that only go to this site or to listed hosts (protection against open redirects)."""

# F-268

import urllib.parse

import veil

__all__ = ["is_safe_url", "safe_redirect"]


def is_safe_url(url, allowed_hosts=(), require_https=False):
    """True when ``url`` is a path on this site (``/x``, not ``//host`` or ``/\\host``) or an ``http(s)`` URL whose host
    is in ``allowed_hosts`` (URLs with user information are refused)."""
    if not url or any(c in url for c in "\r\n\t\x00") or "\\" in url:
        return False
    if url.startswith("/"):
        return not url.startswith("//")
    try:
        u = urllib.parse.urlsplit(url)
    except ValueError:
        return False
    if u.scheme not in (("https",) if require_https else ("http", "https")):
        return False
    if u.username is not None or u.password is not None:
        return False
    host = (u.hostname or "").lower()
    return bool(host) and host in {h.lower() for h in allowed_hosts}


def safe_redirect(url, allowed_hosts=(), fallback="/", status_code=303):
    """A :class:`veil.RedirectResponse` to ``url`` when :func:`is_safe_url`, otherwise to ``fallback``."""
    return veil.RedirectResponse(url if is_safe_url(url, allowed_hosts) else fallback, status_code)
