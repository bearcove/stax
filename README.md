# A sampling profiler for macOS

Originally a fork of [`not-perf`](https://github.com/koute/not-perf) with some
bits of [`samply`](https://github.com/mstange/samply) baked in (for macOS compatibility),
stax is now very much its own codebase.

The goal is pretty much a live version of `Instruments.app`: see on-cpu and off-cpu
stacks as flamegraphs, top-N functions, annotated disassembly etc.

stax uses the private `kdebug` framework, therefore it has access to things `samply`
doesn't, but it also has more moving pieces:

  - a privileged launchd service for kdebug/kperf attachment
  - a non-prileged (same-user) helper to obtain the task port, read/write inferior memory etc.
  
![A screenshot of stax showing a flamegraph on top, top-N functions bottom-left, and annotated disassembly bottom-right](https://github.com/user-attachments/assets/929b4b42-cdd9-4e35-8a91-ee7b029e94e2)
  
## License

Licensed under either of

  * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
  * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
