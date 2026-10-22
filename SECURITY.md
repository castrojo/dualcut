# Security Policy

## Reporting Security Issues

If you discover a security vulnerability in dualcut, please report it
responsibly rather than opening a public issue:

1. **Do not** open a public GitHub issue for a security vulnerability.
2. Use GitHub's private vulnerability reporting: go to the **Security** tab,
   then **Advisories**, and click **"Report a vulnerability"**.
3. Alternatively, contact the maintainers by email with the affected versions,
   an impact description, and a proof of concept if you have one.

We take security seriously and will:

- Acknowledge receipt within 48 hours.
- Provide updates every 7 days or when significant progress is made.
- Work with you to understand and remediate the issue.
- Credit you in any security advisory (unless you prefer anonymity).

## Supported Versions

> **Deprecation notice:** New development has moved to
> [shrimply](https://github.com/soirihiroka/shrimply). This repository is
> retained for its history and existing releases.

Security updates are applied on a best-effort basis to the `main` branch.
Report vulnerabilities in the active project to
[shrimply](https://github.com/soirihiroka/shrimply)'s own security channel.

## Security Considerations

- dualcut is a GNOME video editor with programmatic access via JSON,
  TypeScript, and an HTTP API.
- The editor operates on local files and projects.
- API access should be restricted to trusted scripts and agents.

## Out of Scope

- Vulnerabilities in upstream projects (GStreamer, GTK, Iced, etc.) — report
  those to their respective maintainers.
