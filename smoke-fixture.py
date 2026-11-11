"""Standalone fixture server for legado-server smoke tests (mirrors the
in-process Rust fixture in crates/server/src/source/mod.rs).

Usage: python3 smoke-fixture.py 18097   (binds 127.0.0.1:<port>)
Routes: POST /search, GET /toc, GET /chapter, POST /login, GET /vip,
GET /api/search, GET /api/toc, GET /api/content.
"""
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, status, body, extra=None):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        if extra:
            for k, v in extra.items():
                self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode("utf-8", "replace")
        if self.path == "/search":
            if "page=1" in body:
                books = ('<div class="book"><a class="title" href="/book/1">书一</a>'
                         '<span class="author">甲</span></div>'
                         '<div class="book"><a class="title" href="/book/2">书二</a>'
                         '<span class="author">乙</span></div>')
            elif "page=2" in body:
                books = ('<div class="book"><a class="title" href="/book/3">书三</a>'
                         '<span class="author">丙</span></div>')
            else:
                books = ""
            self._send(200, f"<html><body>{books}</body></html>")
        elif self.path == "/login":
            self._send(200, "welcome ok", {"Set-Cookie": "session=abc123; Path=/; HttpOnly"})
        elif self.path == "/login-form":
            if "username=alice" in body and "password=s3cret" in body:
                self._send(200, "login ok", {"Set-Cookie": "session=abc123; Path=/; HttpOnly"})
            else:
                self._send(401, "bad credentials")
        else:
            self._send(404, "not found")

    def do_GET(self):
        u = urlparse(self.path)
        q = parse_qs(u.query)
        if u.path == "/toc":
            if q.get("page") == ["2"]:
                self._send(200, '<html><body><a class="ch" href="/c/3">第三章</a></body></html>')
            else:
                self._send(200, '<html><body><a class="ch" href="/c/1">第一章</a>'
                            '<a class="ch" href="/c/2">第二章</a>'
                            '<a class="next" href="/toc?page=2">下一页</a></body></html>')
        elif u.path == "/chapter":
            if q.get("page") == ["2"]:
                self._send(200, '<html><body><div id="content"><p>第二页正文。</p></div></body></html>')
            else:
                self._send(200, '<html><body><div id="content"><p>第一页正文。</p></div>'
                            '<a class="next" href="/chapter?page=2">下一页</a></body></html>')
        elif u.path == "/vip":
            if "session=abc123" in (self.headers.get("Cookie") or ""):
                self._send(200, '<html><body><div class="book"><a class="title" href="/book/9">会员书</a>'
                            '<span class="author">丁</span></div></body></html>')
            else:
                self._send(401, "need login")
        elif u.path == "/api/search":
            self._send(200, '{"data":[{"title":"JSON书一","author":"甲","url":"/book/1"},'
                            '{"title":"JSON书二","author":"乙","url":"/book/2"}]}')
        elif u.path == "/api/toc":
            self._send(200, '{"chapters":[{"name":"第一章","url":"/c/1"},{"name":"第二章","url":"/c/2"}]}')
        elif u.path == "/api/content":
            self._send(200, '{"content":"第一段。\\n第二段。"}')
        elif u.path == "/explore":
            self._send(200, '<html><body><div class="book"><a class="title" href="/book/7">发现书</a>'
                        '<span class="author">戊</span></div><div class="book"><a class="title" href="/book/8">发现书二</a>'
                        '<span class="author">己</span></div></body></html>')
        elif u.path == "/login-form":
            self._send(200, '<html><body><form action="/login-form" method="post">'
                        '<input name="username" type="text"/><input name="password" type="password"/>'
                        '<button type="submit">登录</button></form></body></html>')
        elif u.path == "/bookinfo":
            self._send(200, '<html><body><h1 class="title">详情书</h1><span class="author">庚</span></body></html>')
        else:
            self._send(404, "not found")

if __name__ == "__main__":
    HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
