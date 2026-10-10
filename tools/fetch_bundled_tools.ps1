# 按 tools/tool-lock.tsv 下载并校验随包工具（Windows）。
# 用法：tools\fetch_bundled_tools.ps1 -Platform windows-x64 -Dest <目录>
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("windows-x64")]
    [string]$Platform,
    [Parameter(Mandatory = $true)]
    [string]$Dest
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
$Lock = Import-Csv (Join-Path $Root "tools/tool-lock.tsv") -Delimiter "`t"
$BinDir = Join-Path $Dest "bin"
$LicenseDir = Join-Path $Dest "licenses"
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("abb-bundled-tools-" + [guid]::NewGuid().ToString("N"))

New-Item -ItemType Directory -Force -Path $BinDir, $LicenseDir, $Tmp | Out-Null
Copy-Item (Join-Path $Root "tools/licenses/*") $LicenseDir -Force

try {
    $rows = $Lock | Where-Object { $_.platform -eq $Platform }
    if (-not $rows) { throw "lock 中没有平台 $Platform 的工具" }
    if ($rows.Count -ne 4) { throw "lock 中 $Platform 应恰好有 4 个工具，实际 $($rows.Count)" }

    foreach ($row in $rows) {
        Write-Host "  [tools] $($row.tool) $($row.version) -> $($row.output_name)"
        $file = Join-Path $Tmp "$($row.output_name).$($row.package)"
        Invoke-WebRequest -Uri $row.url -OutFile $file -UseBasicParsing

        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant()
        if ($actual -ne $row.sha256.ToLowerInvariant()) {
            throw "SHA256 校验失败：$($row.tool) $($row.version)`n  expected=$($row.sha256)`n  actual  =$actual"
        }

        $out = Join-Path $BinDir $row.output_name
        switch ($row.package) {
            "raw" {
                Copy-Item -LiteralPath $file -Destination $out -Force
            }
            "zip" {
                Add-Type -AssemblyName System.IO.Compression.FileSystem
                $zip = [IO.Compression.ZipFile]::OpenRead($file)
                try {
                    $entry = $zip.Entries | Where-Object {
                        $_.FullName -eq $row.inner_path -or $_.FullName.EndsWith("/$($row.inner_path)")
                    } | Select-Object -First 1
                    if (-not $entry) { throw "包内找不到 $($row.inner_path)（$($row.tool)）" }
                    [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $out, $true)
                }
                finally { $zip.Dispose() }
            }
            default { throw "不支持的 package 类型：$($row.package)（$($row.tool)）" }
        }
        & $out --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "$($row.output_name) --version 退出码 $LASTEXITCODE" }
    }
}
finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "  [tools] 已准备 $($rows.Count) 个工具到 $Dest"
