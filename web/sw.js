// Network-first app shell: always fresh when the server is reachable,
// still opens (showing "unreachable") when it is not. The API is never cached.
const CACHE = "qbm-shell-v1";
// "./" is deliberately absent: it is cached by the first real navigation,
// so a login redirect can never end up stored as the app shell
const SHELL = ["app.css", "app.js", "manifest.webmanifest", "icons/icon.svg"];

self.addEventListener("install", (event) => {
    event.waitUntil(
        caches.open(CACHE)
            .then((c) => c.addAll(SHELL))
            // a login redirect during install must not block activation
            .catch(() => {})
            .then(() => self.skipWaiting())
    );
});

self.addEventListener("activate", (event) => {
    event.waitUntil(
        caches.keys()
            .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
            .then(() => self.clients.claim())
    );
});

self.addEventListener("fetch", (event) => {
    const req = event.request;
    const url = new URL(req.url);

    if (req.method !== "GET" || url.origin !== location.origin || url.pathname.includes("/api/")) {
        return;
    }

    const scope = new URL(self.registration.scope);
    // ?add=... / share-target launches all map to the one cached shell
    const key = req.mode === "navigate" && url.pathname === scope.pathname ? "./" : req;

    event.respondWith(
        fetch(req)
            .then((res) => {
                // only cache real pages, never the login form or a redirect
                if (res.ok && res.type === "basic" && !res.redirected) {
                    const copy = res.clone();
                    caches.open(CACHE).then((c) => c.put(key, copy));
                }
                return res;
            })
            .catch(() => caches.match(key).then((hit) => hit || Response.error()))
    );
});
