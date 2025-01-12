#!/usr/bin/env python3

# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import os
from typing import Any

import requests

BUILDKITE_API_URL = "https://api.buildkite.com/v2"

STATUS_CODE_RATE_LIMIT_EXCEEDED = 429


class RateLimitExceeded(Exception):
    def __init__(self, partial_result: list[Any]):
        self.partial_result = partial_result


def get(request_path: str, params: dict[str, Any]) -> Any:
    headers = {}
    token = os.getenv("BUILDKITE_CI_API_KEY") or os.getenv("BUILDKITE_TOKEN")

    if token is not None and len(token) > 0:
        headers["Authorization"] = f"Bearer {token}"
    else:
        print("Authentication token is not specified or empty!")

    url = f"{BUILDKITE_API_URL}/{request_path}"
    r = requests.get(headers=headers, url=url, params=params)

    if r.status_code == STATUS_CODE_RATE_LIMIT_EXCEEDED:
        raise RateLimitExceeded([])

    return r.json()


def get_multiple(
    request_path: str,
    params: dict[str, Any],
    max_fetches: int | None,
    first_page: int = 1,
) -> list[Any]:
    results = []

    print(f"Starting to fetch data from Buildkite: {request_path}")
    params["page"] = str(first_page)

    fetch_count = 0
    while True:
        try:
            result = get(request_path, params)
        except RateLimitExceeded:
            raise RateLimitExceeded(partial_result=results)

        fetch_count += 1

        if not result:
            print("No further results.")
            break

        if isinstance(result, dict) and result.get("message"):
            raise RuntimeError(f"Something went wrong! ({result['message']})")

        params["page"] = str(int(params["page"]) + 1)

        entry_count = len(result)
        created_at = result[-1]["created_at"]
        print(f"Fetched {entry_count} entries, created at {created_at}.")

        results.extend(result)

        if max_fetches is not None and fetch_count >= max_fetches:
            print("Max fetches reached.")
            break

    return results
