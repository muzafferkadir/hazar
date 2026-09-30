"""Apply captured headers only to their origin, including redirect hops."""
import json
import os
from urllib.parse import urlsplit, urljoin
from yt_dlp.networking.common import RequestHandler
from yt_dlp.networking._urllib import RedirectHandler

__all__ = []

def origin(url):
    parsed = urlsplit(url)
    return (parsed.scheme.lower(), (parsed.hostname or '').lower(), parsed.port or (443 if parsed.scheme == 'https' else 80))

with open(os.environ['HAZAR_CONTEXT_FILE'], encoding='utf-8') as stream:
    context = json.load(stream)
scopes = {origin(item['url']): item['headers'] for item in context.get('scoped_headers', [])}
values = {}
for headers in scopes.values():
    for key, value in headers:
        values.setdefault(key.lower(), set()).add(value)

def scoped(headers, url):
    headers = dict(headers or {})
    target = dict(scopes.get(origin(url), []))
    allowed = {key.lower(): value for key, value in target.items()}
    for key, value in list(headers.items()):
        if value in values.get(key.lower(), ()) and allowed.get(key.lower()) != value:
            del headers[key]
    for key, value in target.items():
        for previous in list(headers):
            if previous.lower() == key.lower():
                del headers[previous]
        headers[key] = value
    return headers

original_headers = RequestHandler._get_headers

def get_headers(self, request):
    return scoped(original_headers(self, request), request.url)
RequestHandler._get_headers = get_headers

original_redirect = RedirectHandler.redirect_request

def redirect(self, req, fp, code, msg, headers, newurl):
    result = original_redirect(self, req, fp, code, msg, headers, newurl)
    if result is not None:
        if origin(req.full_url) != origin(newurl):
            result.headers = {key: value for key, value in result.headers.items() if key.lower() not in ('authorization', 'cookie', 'proxy-authorization')}
        result.headers = scoped(result.headers, newurl)
    return result
RedirectHandler.redirect_request = redirect

try:
    from yt_dlp.networking._requests import RequestsSession
    original_auth = RequestsSession.rebuild_auth
    def rebuild_auth(self, request, response):
        original_auth(self, request, response)
        if origin(response.url) != origin(request.url):
            for key in list(request.headers):
                if key.lower() in ('authorization', 'cookie', 'proxy-authorization'):
                    del request.headers[key]
        updated = scoped(request.headers, request.url)
        request.headers.clear()
        request.headers.update(updated)
    RequestsSession.rebuild_auth = rebuild_auth
except ImportError:
    pass

try:
    from curl_cffi.requests import Session
    from yt_dlp.networking._helper import get_redirect_method
    original_request = Session.request
    def request(self, method, url, **kwargs):
        # Handle redirect hops here so every hop gets its own origin's headers.
        kwargs['allow_redirects'] = False
        for hop in range(6):
            kwargs['headers'] = scoped(kwargs.get('headers'), url)
            response = original_request(self, method, url, **kwargs)
            location = response.headers.get('Location')
            if response.status_code not in (301, 302, 303, 307, 308) or not location or hop == 5:
                return response
            next_method = get_redirect_method(method, response.status_code)
            if next_method != method:
                kwargs['data'] = None
                kwargs['headers'] = {k: v for k, v in kwargs['headers'].items() if k.lower() not in ('content-type', 'content-length')}
            next_url = urljoin(url, location)
            if origin(url) != origin(next_url):
                kwargs['headers'] = {key: value for key, value in kwargs['headers'].items() if key.lower() not in ('authorization', 'cookie', 'proxy-authorization')}
            url = next_url
            if urlsplit(url).scheme not in ('http', 'https'):
                return response
            response.close()
            method = next_method
    if scopes:
        Session.request = request
except ImportError:
    pass
