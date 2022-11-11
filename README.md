# chroni

A mirror backup tool by Cattail Magic Lab

```
Usage: chroni [OPTIONS] <SRC_DIR> <DEST_DIR>

Arguments:
  <SRC_DIR>
          The source directory of the backup task

  <DEST_DIR>
          The destination directory of the backup task

Options:
  -o, --overwrite-mode <MODE>
          Specify the mode for checking if a destination file should be overwritten
          
          [default: fast-comp]

          Possible values:
          - any:       always overwrite
          - fast-comp: overwrite when sizes of the source and the destination are different
          - deep-comp: overwrite when hashes(SHA-1) of the source and the destination are different
          - none:      never overwrite

      --only_newest <GLOB>
          Set the filter of directories which only keep the newest file in it, can be used multiple times

      --dry-run
          Run without actual file operations

  -h, --help
          Print help information (use `-h` for a summary)

  -V, --version
          Print version information
```