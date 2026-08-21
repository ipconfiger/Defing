# Defing on K3s —— 3 节点集群部署清单
#
# 依据 dev_docs/plan-k3s-deployment.md 落地。apply 顺序：
#   1. namespace.yaml secret.yaml entrypoint-configmap.yaml
#   2. headless-service.yaml statefulset.yaml public-service.yaml pdb.yaml
#   3. (可选) ingress.yaml
#
# 前置：按 secret.yaml 头部注释生成 Secret 值；镜像推送到私有 registry 并
# 在 statefulset.yaml 中替换 <registry>/defing:v0.1.0（私有 registry 认证：
# /etc/rancher/k3s/registries.yaml 或 imagePullSecrets）。
