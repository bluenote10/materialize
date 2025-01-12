# Buildkite Annotation Search

This tool allows searching Buildkite annotations in recent builds.

## Usage
```
usage: buildkite-annotation-search [-h]
                                   [--branch BRANCH]
                                   [--fetch-builds {auto,always,never}]
                                   [--fetch-annotations {auto,always,never}]
                                   [--max-build-fetches MAX_BUILD_FETCHES]
                                   [--first-build-page-to-fetch FIRST_BUILD_PAGE_TO_FETCH]
                                   [--max-results MAX_RESULTS]
                                   [--only-one-result-per-build]
                                   [--only-failed-builds]
                                   [--only-failed-build-step-key ONLY_FAILED_BUILD_STEP_KEY]
                                   [--use-regex]
                                   {cleanup,coverage,deploy,deploy-mz-lsp-server,deploy-mz,deploy-website,license,nightly,release-qualification,security,slt,test,www}
                                   pattern
```

### Authentication

You will need an environment variable called `BUILDKITE_TOKEN`, which contains a Buildkite token. Such a token can be
created on https://buildkite.com/user/api-access-tokens/new and will require at least `read_builds`.

## Examples

Builds that have an annotation containing `Error { kind: Db, cause: Some(DbError`

```
bin/buildkite-annotation-search test "Error { kind: Db, cause: Some(DbError"
```

Builds that have an annotation containing `Error` and include a larger number of recent builds

```
bin/buildkite-annotation-search test --max-build-fetches 10 "Error"
```

Builds on branch `main` that have an annotation matching the regex pattern `cannot serve requested as_of AntiChain.*testdrive-materialized-1`

```
bin/buildkite-annotation-search test --branch main --use-regex "cannot serve requested as_of AntiChain.*testdrive-materialized-1"
```

Nightly builds that failed and have an annotation containing `fivetran-destination action=describe`

```
bin/buildkite-annotation-search nightly --only-failed-builds "fivetran-destination action=describe"
```
