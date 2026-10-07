# jlsp: Ultra-Fast, Zero-JVM Java Language Server in Pure Rust

`jlsp` 是一个完全用 Rust 从零实现的超高性能、纯净无 JVM 依赖的 Java 语言服务器（Language Server Protocol 实现）。

- **0 依赖 JVM 启动**：纯静态 Native 二进制，启动时间 < 10ms，常驻内存仅数十 MB。
- **100% 对齐 Eclipse JDT.LS 核心能力**：涵盖 53 项全量语法、类型推断、诊断、重构、快速修复、补全、Maven/Gradle 构建解析与 JDWP 调试。
- **跨平台分发**：原生支持 macOS (Apple Silicon / Intel)、Linux (musl / gnu, x86_64 / aarch64) 与 Windows (x86_64)。

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
