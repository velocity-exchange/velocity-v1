#!/bin/bash

if [ "$1" != "--skip-build" ]
  then
    bash deploy-scripts/build-sbf.sh test && bun run program:idl &&
    cp target/idl/velocity.json packages/sdk/src/idl/
fi

export ANCHOR_WALLET=~/.config/solana/id.json

test_files=(
	order.ts
)

for test_file in ${test_files[@]}; do
  ts-mocha --exit -t 300000 ./tests/${test_file}
done
