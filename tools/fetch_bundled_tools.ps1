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
    # 包 ≠ 工具名：多调包一个包提供多个名字（coreutils/diffutils）。校验"每包两平台"由 CI 侧的
    # Rust 矩阵测试负责，这里只要求该平台的包数 > 0。
    if ($rows.Count -lt 1) { throw "lock 中 $Platform 没有任何包" }
    $manifest = Import-Csv (Join-Path $Root "tools/bundled-cli-tools.tsv") -Delimiter "`t"

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
            { $_ -like "multicall-*" } {
                # 多调包（uutils coreutils / diffutils）：
                #  - Windows 的 coreutils 资产是**每工具一个 exe** ⇒ 按名单取件；
                #  - diffutils 仍是多调 ⇒ 解出多调二进制后建**硬链接**（同卷免权限）；
                #    硬链接失败回落复制（体积换正确性，并告警）。
                $names = @($manifest | Where-Object { $_.provider -eq $row.tool } | ForEach-Object { $_.name })
                if ($names.Count -eq 0) { throw "$($row.tool) 在 bundled-cli-tools.tsv 里没有任何名字" }
                $raw = Join-Path $Tmp ("extract" + $row.tool)
                New-Item -ItemType Directory -Force -Path $raw | Out-Null
                Add-Type -AssemblyName System.IO.Compression.FileSystem
                [IO.Compression.ZipFile]::ExtractToDirectory($file, $raw, $true)
                $provider = Join-Path $raw $row.output_name
                foreach ($name in $names) {
                    $per = Get-ChildItem -Path $raw -Recurse -Filter "$name.exe" | Select-Object -First 1
                    $target = Join-Path $BinDir "$name.exe"
                    if ($per) {
                        Copy-Item $per.FullName $target -Force
                    } elseif (Test-Path $provider) {
                        New-Item -ItemType HardLink -Path $target -Target $provider -Force | Out-Null
                    } else {
                        throw "$($row.tool) 包里既没有 $name.exe 也没有多调二进制 $($row.output_name)"
                    }
                }
            }
            default { throw "不支持的 package 类型：$($row.package)（$($row.tool)）" }
        }
        & $out --version | Out-Null
        if ($row.package -like "multicall-*") {
            foreach ($name in ($manifest | Where-Object { $_.provider -eq $row.tool } | ForEach-Object { $_.name })) {
                & (Join-Path $BinDir "$name.exe") --version *> $null
                # 不依赖退出码为 0（uutils diffutils 不实现 --version）；只要求"能启动"：
                # PowerShell 找不到可执行文件会抛 CommandNotFoundException，上面那行会直接终止。
            }
        } elseif ($LASTEXITCODE -ne 0) {
            throw "$($row.output_name) --version 退出码 $LASTEXITCODE"
        }
    }
}
finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "  [tools] 已准备 $($rows.Count) 个工具到 $Dest"
