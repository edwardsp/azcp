# Installing mpi-operator on AKS

`azcp-cluster`'s [recommended AKS deployment](../../docs/cluster-aks.md)
uses the [Kubeflow MPI Operator](https://github.com/kubeflow/mpi-operator).
This document is the minimal install + verification path.

If your cluster already has mpi-operator installed (some managed AI/HPC
AKS distributions ship it pre-installed), skip to "Verifying the install."

## 1. Install

Pin to a release. v0.6.0 is the current stable (as of mid-2025); check
the [releases page](https://github.com/kubeflow/mpi-operator/releases)
for the latest.

```bash
kubectl apply --server-side -f \
  https://raw.githubusercontent.com/kubeflow/mpi-operator/v0.6.0/deploy/v2beta1/mpi-operator.yaml
```

This installs:

- The `MPIJob` CRD (`mpijobs.kubeflow.org/v2beta1`).
- The mpi-operator controller in the `mpi-operator` namespace.
- An RBAC bundle scoped to managing MPIJobs cluster-wide.

`--server-side` is recommended — the manifest is large and the operator's
CRD has historically tripped client-side apply size limits.

## 2. Verifying the install

```bash
# CRD is present:
kubectl get crd mpijobs.kubeflow.org

# Controller is running:
kubectl get pods -n mpi-operator
# NAME                            READY   STATUS    RESTARTS   AGE
# mpi-operator-7c8d9c7f4d-xxxxx   1/1     Running   0          1m

# Controller logs are clean (no permission errors etc):
kubectl logs -n mpi-operator -l app=mpi-operator --tail=20
```

A quick smoke MPIJob (no fabric required, just confirms the controller
schedules launcher+worker correctly):

```bash
cat <<'EOF' | kubectl apply -f -
apiVersion: kubeflow.org/v2beta1
kind: MPIJob
metadata:
  name: mpi-smoke
spec:
  slotsPerWorker: 1
  launcherCreationPolicy: WaitForWorkersReady
  mpiReplicaSpecs:
    Launcher:
      replicas: 1
      template:
        spec:
          restartPolicy: OnFailure
          containers:
            - name: launcher
              image: mpioperator/mpi-pi:openmpi
              command: ["mpirun", "-n", "2", "/home/mpiuser/pi"]
    Worker:
      replicas: 2
      template:
        spec:
          containers:
            - name: worker
              image: mpioperator/mpi-pi:openmpi
              resources:
                requests:
                  cpu: 100m
                  memory: 128Mi
EOF

kubectl logs -f -l training.kubeflow.org/job-role=launcher
# Should print a Pi approximation and exit 0.

kubectl delete mpijob mpi-smoke
```

If the launcher logs print `pi is approximately 3.14...`, you're ready
to deploy `azcp-cluster`.

## 3. Common install issues

- **`unable to recognize ... no matches for kind "MPIJob"`**: the apply
  succeeded but kubectl's discovery cache is stale. Run `kubectl
  api-resources | grep mpijob` to confirm; if it shows up, retry the
  apply.
- **`mpi-operator` pod stuck in `ImagePullBackOff` on a private cluster**:
  the manifest pulls from `mpioperator/mpi-operator` on Docker Hub.
  Mirror to ACR: `docker pull mpioperator/mpi-operator:v0.6.0 && docker
  tag ... && docker push <acr>/mpioperator/mpi-operator:v0.6.0`, then
  edit the deployment image.
- **RBAC errors creating MPIJob in a namespace**: the operator manages
  MPIJobs cluster-wide but submitting users still need `create
  mpijobs.kubeflow.org` in the target namespace. Add an appropriate
  RoleBinding for your team.

## 4. Uninstalling

```bash
kubectl delete -f \
  https://raw.githubusercontent.com/kubeflow/mpi-operator/v0.6.0/deploy/v2beta1/mpi-operator.yaml
```

This deletes the controller, CRD, and all `MPIJob` resources cluster-wide.
Make sure there's nothing live before running it.
