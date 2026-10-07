# Workbench sandbox libraries

Pinned, browserified JavaScript libraries loaded lazily inside the isolated Boa
worker. No host Node.js require or filesystem/network module access is exposed.

Rebuild with Node.js using `npm ci --ignore-scripts` followed by `node build.cjs`
in this directory. Commit the lockfile, generated `.min.js` files and
`THIRD_PARTY_NOTICES.txt`. Ordinary Cargo builds do not invoke npm or download
packages. Build dependencies and their notices are included for reproducibility.

The exposed modules are Ajv 6.12.6, Chai 4.5.0, Cheerio 0.22.0 (matching the
Postman guide API), CryptoJS 4.2.0, Moment 2.30.1 and xml2js 0.6.2. CryptoJS random
values come from the host OS random generator; it has no host crypto/module
access. Resource, cancellation, script wall-clock and subrequest transport
limits remain in force. Ajv external references do not fetch URLs.
