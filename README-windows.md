# Windows support

The current rust crate will not compile on windows due to the `protobuf-src` crate since it expects to run various shell scripts. The issue is described in the following issue:
- https://github.com/MaterializeInc/rust-protobuf-native/issues/4

There are some workarounds to fix this:
1. Using the latest version of `protobuf-src`, supposedly fixes this (have not tested this).
2. Use one of the solutions in the above issue
    - https://github.com/MystenLabs/sui/issues/5228
    - https://github.com/MystenLabs/sui/pull/5249

# Current fix
We are going with solution #2 and removing the `protobuf-src` build dependency when building on windows. The main changes are:
1. Setting custom build dependencies for windows target in the `yellowstone-grpc-proto` library
2. Adding a custom config filter in the `yellowstone-grpc-proto` build script