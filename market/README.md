# 市场条目（手动 PR 用）

本目录提供向 [MicYou-Plugins](https://github.com/MicYou-Dev/MicYou-Plugins) 市场仓库
提交条目所需的材料：`opss.focus-capture/`（plugin.json + 源码 + README + LICENSE，
符合 marketplace-policy 开源准入）。

## 首次提交（手动）
1. 在 MicYou-Plugins 仓库创建 `plugin/opss.focus-capture/`，拷入本目录内容；
2. 运行 `npx tsx scripts/generate_catalog.ts` 重生成 `index.json`；
3. 提 PR。条目 `downloadUrl` 为 **evergreen 别名**
   （`releases/latest/download/plugin.zip`，每次 release 同字节上传），
   因此**合并后无需逐版本再提市场 PR**；应用内更新走 `updateUrl`（市场 Pages manifest）。

## 可选：逐版本市场 PR
若希望市场条目版本号与 release 同步：每次 release 的资产中含 `market-entry.json`
（downloadUrl 指向版本化 zip），用它替换市场条目 plugin.json 并重生成 index.json 后提 PR。
