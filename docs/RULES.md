# 代码规则

## 依赖
除了来自git的依赖外，其他依赖必须是最新版本，并尽量确认是较活跃的项目。

## 代码规范
代码阶段性完成后应使用`cargo fmt`格式化代码。并使用`cargo clippy --workspace -- -D warnings`检查代码质量。