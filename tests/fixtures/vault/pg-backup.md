---
id: pg-backup
title: "Nightly Postgres dump to S3"
topic: postgres
---

# Nightly Postgres dump to S3 {#root}

related:: [[restic-backup#root]]

A cron job dumps the database and uploads it to a versioned bucket.

## Retention {#retention}

Keep fourteen dailies and eight weeklies, pruned by a lifecycle rule.
