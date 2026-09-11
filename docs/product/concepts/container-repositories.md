# Container Repositories

Kloudlite hosts container images too. When the refund fix merges and CI builds a
new `payments` image, that image is pushed to a container repository in
Kloudlite, and every [environment](environments.md) that runs `payments` pulls it
from there.

Holding both ends means an environment does not depend on an external registry
to come up, and a pipeline does not need registry credentials handed between
services.

## Where images sit in the loop

Images are how an environment runs a component *normally* — not how you iterate
on one.

While you are working on `payments`, no image is built at all. Your workspace
[intercepts](connections.md) the component and the environment's traffic goes to
your running process. The `payments` image only matters once the fix has merged
and every environment should be running it by default — and it matters for `api`,
`web`, and `worker` the whole time, because those are the components you are not
touching and the environment runs them from their images.

So the split is: images for the components you are not changing, intercept for
the one you are.

<!-- Open: image naming and tagging scheme; retention and garbage collection;
     pulling from external registries; how CI/CD pushes here. -->
