#!/usr/bin/env bash
# Keep reference/aws-cli at the aws-cli release recorded in upstream.lock.
#
#   scripts/upstream.sh fetch          check out the locked release
#   scripts/upstream.sh latest         print the latest 2.x.y release tag
#   scripts/upstream.sh update [TAG]   move to TAG (default: latest), rewrite
#                                      upstream.lock, refresh assets/cli.json
#                                      and write a summary of the model
#                                      changes to target/upstream-summary.md
#   scripts/upstream.sh fixtures       refresh the test fixtures that come
#                                      from reference/aws-cli (copied test
#                                      suites, and skeleton / output / help
#                                      generated with its CLI; needs uv and
#                                      mandoc)
set -euo pipefail
cd "$(dirname "$0")/.."

REPO=https://github.com/aws/aws-cli
REF=reference/aws-cli
LOCK=upstream.lock
DATA=$REF/awscli/botocore/data
FIXTURES=tests/fixtures
VENV=target/awscli-venv

lock_value() {
    sed -n "s/^$1 *= *//p" "$LOCK"
}

latest_tag() {
    git ls-remote --tags --refs "$REPO" 'refs/tags/2.*' |
        sed 's#.*refs/tags/##' |
        grep -E '^2\.[0-9]+\.[0-9]+$' |
        sort -t. -k1,1n -k2,2n -k3,3n |
        tail -n 1
}

# Check out a release tag (shallow) and print its commit.
checkout() {
    local tag=$1
    if [ ! -d "$REF/.git" ]; then
        git clone --quiet --depth 1 --branch "$tag" "$REPO" "$REF"
    else
        git -C "$REF" fetch --quiet --depth 1 origin "+refs/tags/$tag:refs/tags/$tag"
        git -C "$REF" checkout --quiet --detach "refs/tags/$tag"
    fi
    git -C "$REF" rev-parse HEAD
}

write_lock() {
    local tag=$1 commit=$2
    {
        sed -n '/^#/p' "$LOCK"
        echo "tag = $tag"
        echo "commit = $commit"
    } > "$LOCK.tmp"
    mv "$LOCK.tmp" "$LOCK"
}

cmd=${1:-}
case "$cmd" in
fetch)
    tag=$(lock_value tag)
    commit=$(checkout "$tag")
    if [ "$commit" != "$(lock_value commit)" ]; then
        echo "error: tag $tag is now $commit, but $LOCK records $(lock_value commit)" >&2
        exit 1
    fi
    echo "reference/aws-cli is at aws-cli $tag ($commit)"
    ;;
latest)
    latest_tag
    ;;
update)
    old_tag=$(lock_value tag)
    new_tag=${2:-$(latest_tag)}
    if [ "$new_tag" = "$old_tag" ]; then
        echo "already at aws-cli $old_tag"
        exit 0
    fi
    mkdir -p target
    checkout "$old_tag" > /dev/null
    python3 scripts/upstream_summary.py snapshot "$DATA" target/upstream-old.json
    commit=$(checkout "$new_tag")
    python3 scripts/upstream_summary.py snapshot "$DATA" target/upstream-new.json
    write_lock "$new_tag" "$commit"
    # Global option docs used by help (src/help.rs embeds this copy).
    cp "$REF/awscli/data/cli.json" assets/cli.json
    python3 scripts/upstream_summary.py report \
        target/upstream-old.json target/upstream-new.json \
        "$REF/CHANGELOG.rst" "$old_tag" "$new_tag" > target/upstream-summary.md
    echo "updated aws-cli $old_tag -> $new_tag ($commit); summary in target/upstream-summary.md"
    ;;
fixtures)
    # Test suites copied as they are.
    rsync -a --delete "$REF/tests/functional/botocore/endpoint-rules/" "$FIXTURES/endpoint-rules/"
    for dir in input output; do
        rsync -a --delete "$REF/tests/unit/botocore/protocols/$dir/" "$FIXTURES/protocols/$dir/"
    done
    cp "$REF/tests/unit/botocore/protocols/protocol-tests-ignore-list.json" "$FIXTURES/protocols/"
    # Output of the reference CLI. Only its hash-locked dependencies are
    # installed; awscli itself runs from the checkout, so nothing of it
    # (such as its build backend) runs at install time.
    mkdir -p target
    uv venv --quiet --clear -p 3.12 "$VENV"
    VIRTUAL_ENV=$VENV uv pip install --quiet --require-hashes \
        -r "$REF/requirements/download-deps/system-sandbox-lock.txt"
    export AWS_DEFAULT_REGION=us-east-1 PYTHONPATH=$REF
    "$VENV/bin/python" "$FIXTURES/skeleton/generate.py" > "$FIXTURES/skeleton/cases.json"
    "$VENV/bin/python" "$FIXTURES/output/generate.py" > "$FIXTURES/output/cases.json"
    "$VENV/bin/python" "$FIXTURES/help/generate.py" "$FIXTURES/help"
    echo "refreshed the fixtures from aws-cli $(lock_value tag)"
    ;;
*)
    sed -n '2,14p' "$0" >&2
    exit 2
    ;;
esac
