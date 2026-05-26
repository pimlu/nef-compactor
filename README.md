# nef-compactor

nef-compactor losslessly compresses classic Nikon NEF raws, better than Nikon does it. Produces `.CNEF` files that are smaller than the original `.NEF` raws but can be decompressed back into the exact `.NEF` raw.  No data is deleted.

The compression ratio depends on the content of the files -- fundamentally noise is incompressible, so the less noise, the better:

 * Bright/low ISO: Should fit something like 40% more photos on the same SD card.
 * Dark/noisy: Might fit 10%-25% more photos on the same SD card.

It works by re-encoding the raw data/JPEG thumbnails with lossless JPEG XL, which is completely free for open source software to use.

## HE/HE★ FAQ

nef-compactor *does not support HE/HE★* files, since no open-source software does.  It's patent-encumbered, meaning if you're not careful, even a clean room reverse engineered implementation can violate somebody's patents.

### Why is HE/HE★ smaller than nef-compactor?

It's actually a lossy format, they're throwing data away to get that compression.

## Usage

```
# Compress a single file
nef-compactor compress photo.NEF

# Preview compression ratios without writing anything
nef-compactor compress --dry-run -R /Volumes/Foo/DCIM/100NZ502

# Compress a directory (8 threads)
nef-compactor compress -R -j8 /Volumes/Foo/DCIM/100NZ502

# Compress and remove originals
nef-compactor compress -R -j8 --rm /Volumes/Foo/DCIM/100NZ502

# Decompress a single file
nef-compactor decompress photo.CNEF

# Decompress a directory
nef-compactor decompress -R -j8 ~/Photos/

# Verbose output showing per-segment breakdown
nef-compactor compress -R -j8 --dry-run -v /Volumes/Foo/DCIM/100NZ502

```

### Flags

| Flag | Description |
|---|---|
| `-R` | Operate on all NEF/CNEF files in a directory |
| `-j N` | Number of parallel threads |
| `--dry-run` | Show ratios without writing files |
| `--skip-verify` | Skip roundtrip verification (faster, less safe) |
| `--rm` | Remove original `.NEF` after verified, fsynced write. Incompatible with `--skip-verify` |
| `-v` | Show per-segment compression breakdown |

## Disclaimer

I've done my best to ensure this can't destroy your data.  nef-compactor verifies that decompression restores your exact original `.NEF` file before deleting it.  It also flushes the `.CNEF` to disk before rm'ing the `.NEF` file.  But I'm not perfect, and I wrote big chunks of this with the help of Claude.

So of course, use your own judgement and verify this works yourself before commiting large photo libraries to it.
