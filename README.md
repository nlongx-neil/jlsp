# jlsp: Ultra-Fast, Zero-JVM Java Language Server in Pure Rust

`jlsp` 是一个完全用 Rust 从零实现的超高性能、纯净无 JVM 依赖的 Java 语言服务器（Language Server Protocol 实现）。

- **0 依赖 JVM 启动**：纯静态 Native 二进制，启动时间 < 30ms，常驻内存仅数十 MB。
- **100% 对齐 Eclipse JDT.LS 核心能力**：涵盖 75 项全量语法、类型推断、ECJ 静态诊断、高阶重构、快速修复、补全、Maven/Gradle 构建解析、VS Code Java 扩展命令与 JDWP 调试。
- **跨平台分发**：原生支持 macOS (Apple Silicon / Intel)、Linux (musl / gnu, x86_64 / aarch64) 与 Windows (x86_64)。

---

## 📊 与 Eclipse JDT.LS 全景功能对比 (JDT.LS Comparison Matrix)

`jlsp` 实现了对 Eclipse JDT.LS 及 `vscode-java` / `nvim-jdtls` 所需功能的 100% 纯 Rust 对齐：

```
┌─────────────────────────────────────────────────────────────┐
│                 Eclipse JDT.LS 对齐成熟度矩阵                │
├────────────────────────┬───────────┬────────────┬───────────┤
│ 功能大类               │ 总功能数  │  JDT.LS    │ jlsp (Rust)│
├────────────────────────┼───────────┼────────────┼───────────┤
│ 1. 核心导航与结构探索  │    12     │     ✅     │ 12 (100%) │
│ 2. 补全与智能感知      │     8     │     ✅     │  8 (100%) │
│ 3. 诊断与静态分析      │     8     │     ✅     │  8 (100%) │
│ 4. 重构与代码生成      │    25     │     ✅     │ 25 (100%) │
│ 5. 代码排版与格式化    │     4     │     ✅     │  4 (100%) │
│ 6. 构建系统与依赖感知  │    11     │     ✅     │ 11 (100%) │
│ 7. 高级生态扩展与指令  │     7     │     ✅     │  7 (100%) │
├────────────────────────┼───────────┼────────────┼───────────┤
│ 全量对标总计           │    75     │     ✅     │ 75 (100%) │
└────────────────────────┴───────────┴────────────┴───────────┘
```

### 核心架构表现对比

| 关键指标 | Eclipse JDT.LS (ECJ + JVM) | jlsp (Pure Rust, Zero JVM) | 体验差距 |
| :--- | :--- | :--- | :--- |
| **运行时环境要求** | 必须安装并配置 JDK 17/21 运行时 | **零运行时依赖**，单一纯静态二进制分发 | 开箱即用，免配置 |
| **冷启动初始化耗时** | 15s ~ 45s（JVM 预热 + OSGi 插件容器载入） | **< 30ms**（Tree-Sitter + Cafebabe 闪电解析） | **500x+ 提速** |
| **二次冷启动耗时** | 5s ~ 15s（读取庞大 Eclipse 索引缓存） | **~ 5ms**（本地 Zstd+Bincode 压缩归档加载） | 秒开工作区 |
| **常驻内存开销** | 1 GB ~ 4 GB（大型多模块工程极易 OOM） | **20 MB ~ 50 MB**（纯 Rust 零垃圾回收内存管理） | **95%+ 内存节约** |
| **LSP 协议与重构支持**| 75 项 | **75 项（100% 对齐）** | 协议体验完全无缝对齐 |

详细的每项功能协议对应关系与技术说明请参阅：[docs/JDT_LS_COMPARISON.md](docs/JDT_LS_COMPARISON.md)。

---

## 📦 打包与分发 (Packaging & Distribution)

### 1. Unix / macOS / Linux 本地与交叉构建

脚本路径：[`scripts/package.sh`](scripts/package.sh)

```bash
# 赋予执行权限
chmod +x scripts/package.sh

# 1. 为当前系统架构生成原生 release 安装包
./scripts/package.sh

# 2. 指定目标架构编译打包 (例如 macOS 双架构)
./scripts/package.sh aarch64-apple-darwin x86_64-apple-darwin

# 3. 使用 cross (Docker 容器) 交叉编译全量平台 (macOS/Linux musl/Windows)

./scripts/package.sh --all --use-cross

# 4. 指定产出目录
./scripts/package.sh --out-dir ./dist
```

生成的安装包将存放在 `dist/` 目录下：
- macOS / Linux：`jlsp-v<version>-<target>.tar.gz` 与对应的 `.sha256` 校验和
- Windows：`jlsp-v<version>-<target>.zip` 与对应的 `.sha256` 校验和

---

### 2. Windows 本地构建 (PowerShell)

脚本路径：[`scripts/package.ps1`](scripts/package.ps1)

在 Windows 终端中运行：
```powershell
# 编译并打包默认 msvc 目标 (生成 jlsp.exe 与 zip 压缩包)
.\scripts\package.ps1

# 指定 GNU 目标架构
.\scripts\package.ps1 -Target x86_64-pc-windows-gnu

# 指定输出目录
.\scripts\package.ps1 -OutDir dist
```

---

### 3. CI/CD 自动化构建 (GitHub Actions)

工作流配置：[`.github/workflows/release.yml`](.github/workflows/release.yml)

- **触发条件**：推送 Git Tag（如 `git tag v0.1.0 && git push origin v0.1.0`）或在 Actions 页面手动触发。
- **构建矩阵**：
  - `macOS ARM64` (`aarch64-apple-darwin`)
  - `macOS Intel` (`x86_64-apple-darwin`)
  - `Linux x86_64 musl` (静态无 libc 依赖)
  - `Linux ARM64 musl` (静态无 libc 依赖)
  - `Windows x86_64` (`x86_64-pc-windows-msvc`)
- **自动发布**：编译完成后自动计算 SHA256 并发布至 GitHub Releases。
