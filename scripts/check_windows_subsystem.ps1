param(
    [Parameter(Mandatory = $true)]
    [string[]]$Path
)

$ErrorActionPreference = 'Stop'

foreach ($executable in $Path) {
    $resolved = (Resolve-Path -LiteralPath $executable).Path
    $stream = [IO.File]::OpenRead($resolved)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw "Not a Windows executable: $resolved"
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "Invalid PE signature: $resolved"
        }
        $stream.Position = $peOffset + 24
        $magic = $reader.ReadUInt16()
        if ($magic -notin @(0x10B, 0x20B)) {
            throw "Unsupported PE optional header: $resolved"
        }
        # Subsystem has the same offset in PE32 and PE32+ optional headers.
        $stream.Position = $peOffset + 24 + 68
        $subsystem = $reader.ReadUInt16()
        if ($subsystem -ne 2) {
            throw "Expected Windows GUI subsystem (2), got $subsystem`: $resolved"
        }
        Write-Output "Windows GUI subsystem verified: $resolved"
    } finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}
