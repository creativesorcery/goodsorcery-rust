# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub's private vulnerability reporting:

**https://github.com/creativesorcery/goodsorcery-rust/security/advisories/new**

Do not open a public issue, pull request or discussion for a vulnerability. Include what you found, the affected version (or commit), and steps or code to reproduce it. We will acknowledge the report, keep you posted while we work on a fix, and credit you in the advisory unless you would rather stay anonymous.

Problems in the Good Sorcery server (the hosted service at app.goodsorcery.com or a self-hosted instance) belong to [goodsorcery_web](https://github.com/creativesorcery/goodsorcery_web/security/advisories/new).

## Supported versions

Fixes go into the latest released version on crates.io.

## Scope

This crate runs inside your app and sends error and performance data, with your API key, to a Good Sorcery server. Of particular interest:

- the API key or captured data reaching anyone other than the configured endpoint (for example through redirects, logs or proxies);
- values that should be filtered (passwords, tokens, cookies, request bodies) being sent;
- the client panicking, blocking or deadlocking inside the host app.
