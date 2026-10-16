`brave-web.json` is a minimal response shaped from the public Brave Web Search
reference, retrieved 2026-10-16:
https://api-dashboard.search.brave.com/api-reference/web/search/get

It contains a public search-engine landing page and one-line context only. It is
not a production response. `brave-web.json.gz` is the same fixture compressed with
gzip (mtime=0), exercising the HTTP decoding path. Previously Brave advertised
`Accept-Encoding: gzip` while reqwest had no gzip decoder enabled, making a
compressed JSON response fail decoding. No real API key was used.
