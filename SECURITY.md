# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in Bladebro, please report it privately:

1. **Do not open a public issue.**
2. Email the maintainer directly: **bhandaribishesh879@gmail.com**
3. Include a description of the vulnerability, steps to reproduce, and potential impact.

You will receive a response within 48 hours. If the vulnerability is confirmed, a fix will be prioritized and a security advisory published via GitHub Security Advisories.

## Scope

- Vulnerabilities in Bladebro's code (Rust source)
- Stealth bypasses that would expose agent sessions to detection
- Path traversal or injection in session save/load
- CDP connection handling (e.g., Chrome death, resource exhaustion)

## Out of scope

- Using Bladebro to bypass website terms of service
- Bot detection on sites that require CAPTCHA solving (Bladebro detects and reports honestly — it does not solve)
- Vulnerabilities in Chromium itself (report to the Chromium project)
