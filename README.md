# Buck2 Change Detector

For a [Buck2 project](https://buck2.build), given which files have changed,
figure out which targets have changed. This process is also known as _target
determination_. The primary use case is for building a CI system.

## Project structure

The project is structed as three binaries:

- `targets` to dump the necessary information from the Buck2 graph.
- `btd` to take the information and figure out the impacted targets.
- `supertd` which provides `supertd targets` and `supertd btd` so you can deploy
  it as one single binary.

## Building a CI

When a PR/diff arrives, you would typically:

1. Get a list of the changed files, using your version control system.
2. Dump the Buck2 targets and dependencies _before_ applying the changes (the
   base state), using this repos `targets` binary.
3. Dump the Buck2 targets and dependencies _after_ applying the changes (the
   diff state), using this repos `targets` binary.
4. From that information figure out which targets might be impacted, using this
   repos `btd` binary.
5. Run a `buck2 build $TARGETS && buck2 test $TARGETS`.

A complete example up to step 4, with the full command line flags, is available
in [the BTD Readme](btd/README.md).

### Trimming the build

For very large repos, sometimes a change in a key dependency will cause an
infeasible number of targets to be generated. The output from step 4 includes a
`depth` parameter on each target (if you use `--json`), so you may wish to avoid
recompiling targets many steps away - at the risk of potentially allowing a
breakage into the repo.

### Optimising the process

In many cases steps 1 and 4 will be fairly quick, but steps 2 and 3 can be quite
slow. BTD provides many mechanisms for caching and reusing partial information,
described in [the BTD Readme](btd/README.md).

### Executing the `buck2 build && buck2 test`

This repo doesn't provide any support for running the subsequent `build`/`test`.
At Meta we use a project called Citadel which uses Buck2 labels to annotate
which projects are expected to compile on Linux/Mac/Windows, with what
optimisation settings, where the tests might run, what cross-compilation is
required. We hope to release Citadel in due course, but a simple `buck2` command
probably suffices for most users.

## Similar projects

For Bazel there are two projects that perform aspects of this process:

- [Bazel Diff](https://github.com/Tinder/bazel-diff)
- [Bazel Target Determinator](https://github.com/bazel-contrib/target-determinator)

## License

Buck2 Change Detector is licensed under both the MIT license and Apache-2.0
license; the exact terms can be found in the [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE) files, respectively.
