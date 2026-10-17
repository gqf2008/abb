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
# 目标目录可能被上一次构建复用：先清空 bin/。否则已下架的工具（wassette 2026-10-10 废弃、
# 旧 binutils 的 objcopy/objdump/ld/size、peports）会残留下来一起被打包；
# check_bundled_tools.sh 的「期望集合 == 实际目录」也会红。
if (Test-Path $BinDir) { Get-ChildItem -LiteralPath $BinDir -File -Force | Remove-Item -Force }
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
                # 多调包（uutils coreutils / diffutils / busybox-w32）——与 fetch_bundled_tools.sh 的
                # 分支**同构**（两处逻辑必须一致，否则一个平台绿一个平台红）：
                #   ① 先把**包产物** output_name 落到 bin/（裸 exe 就是它自己；归档里按名字找，
                #      可能在带目录的前缀下——uutils 的 zip 就是这样）；
                #   ② 再按清单为每个名字落文件：优先"包内同名文件"（coreutils 的 Windows zip 是
                #      每工具一个 exe），否则硬链接到包产物（diffutils/busybox 是多调），
                #      硬链接失败回落复制；
                #   ③ 两者都拿不到就报错（绝不静默少一个工具）。
                $names = @($manifest | Where-Object {
                    $_.provider -eq $row.tool -and ($_.platform -eq "all" -or $_.platform -eq $Platform)
                } | ForEach-Object { $_.name })
                if ($names.Count -eq 0) { throw "$($row.tool) 在 bundled-cli-tools.tsv 里没有任何名字（$Platform）" }
                $raw = Join-Path $Tmp ("extract\" + $row.tool)
                New-Item -ItemType Directory -Force -Path $raw | Out-Null
                if ($row.package -eq "multicall-raw") {
                    Copy-Item $file (Join-Path $raw $row.output_name) -Force
                } else {
                    Add-Type -AssemblyName System.IO.Compression.FileSystem
                    [IO.Compression.ZipFile]::ExtractToDirectory($file, $raw, $true)
                }
                $product = Get-ChildItem -Path $raw -Recurse -File -Filter $row.output_name | Select-Object -First 1
                if (-not $product) { throw "$($row.tool) 包里找不到包产物 $($row.output_name)" }
                Copy-Item $product.FullName (Join-Path $BinDir $row.output_name) -Force
                foreach ($name in $names) {
                    $target = Join-Path $BinDir "$name.exe"
                    if (Test-Path $target) { continue }
                    $per = Get-ChildItem -Path $raw -Recurse -File -Filter "$name.exe" | Select-Object -First 1
                    if ($per) {
                        Copy-Item $per.FullName $target -Force
                    } else {
                        try {
                            New-Item -ItemType HardLink -Path $target -Target (Join-Path $BinDir $row.output_name) -Force | Out-Null
                        } catch {
                            Copy-Item (Join-Path $BinDir $row.output_name) $target -Force
                        }
                    }
                }
            }
            default { throw "不支持的 package 类型：$($row.package)（$($row.tool)）" }
        }
        if ($row.package -like "multicall-*") {
            foreach ($name in ($manifest | Where-Object {
                $_.provider -eq $row.tool -and ($_.platform -eq "all" -or $_.platform -eq $Platform)
            } | ForEach-Object { $_.name })) {
                $exe = Join-Path $BinDir "$name.exe"
                if (-not (Test-Path $exe)) { throw "缺产物：$exe" }
                # 只要求"能启动"：不依赖退出码（uutils diffutils 不实现 --version）。
                & $exe --version *> $null
            }
        } else {
            $out = Join-Path $BinDir $row.output_name
            if (-not (Test-Path $out)) { throw "缺产物：$out" }
            & $out --version *> $null
            if ($LASTEXITCODE -ne 0) { throw "$($row.output_name) --version 退出码 $LASTEXITCODE" }
        }
    }
}
finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "  [tools] 已准备 $($rows.Count) 个工具到 $Dest"

# 显式以 0 退出（2026-10-12 修，卡 abb-windows-ps1-exit-code-leak-20261012）：
# `pwsh -File script.ps1` 结束时，若最后一条**原生命令**留下了非 0 的 `$LASTEXITCODE`，进程退出码就是它。
# 而本脚本倒数第二个动作是多调包 smoke（`& $exe --version`，**故意容忍非 0**：uutils diffutils 不实现
# `--version`，busybox 的多数 applet 也不认它），`Write-Host` 是 cmdlet 不会重置该变量 ⇒ 脚本"成功"却以 1
# 退出，CI 步骤失败、release job 被跳过（v2.23.115 的 windows job 就是这样挂的）。
# POSIX 侧 `fetch_bundled_tools.sh` 末尾是同义的 `echo`（内建，返回 0）故不中招——别再把"两脚本同构"当等价。
# 出错路径不受影响：仍由 throw / `exit 1` 负责（`$ErrorActionPreference = "Stop"`）。
exit 0
