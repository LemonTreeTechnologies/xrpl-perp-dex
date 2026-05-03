# Moono-repo proposal

## Introduction

We currently have 3 repositories for our project:
- `frontend` - contains the code for the frontend application
- `backend` - contains the code for the backend application
- `enclave` - contains the code for the enclave application

This structure has some advantages, such as separation of concerns and easier management of dependencies. However, it also has some disadvantages, such as increased complexity in managing multiple repositories and potential issues with versioning and synchronization.

## Proposal

I propose that we create a master repo above this which will contain all three repositories as submodules. This way, we can maintain the separation of concerns while also simplifying the management of the codebase.

The structure would look like this:

```
root/
├── frontend/
├── backend/
├── enclave/
```

## Benefits
This allows us to effectively use the mono-repo approach while still maintaining the benefits of having separate repositories for each component. It also simplifies the process of managing dependencies and versioning, as we can use a single versioning scheme for the entire codebase. Additionally, it allows for easier collaboration and communication between teams working on different components, as they will all be working within the same repository. Overall, this approach can help to improve the efficiency and effectiveness of our development process while still maintaining the benefits of separation of concerns.


We would then be able to use the master repo to manage the deployment processes using ansible for the deployment and configuration of the different components.

This approach ensures that the deployment and monitoring processes are independent of the codebase.

## Requirements for the master repo
- The master repo should be able to manage the deployment processes using ansible for the deployment and configuration of the different components.


## Workflows
- We will have 3 different environments: `dev`, `staging`, and `production`. Each environment will have its own configuration files and deployment processes.

### Dev environment

This environment will deploy the entire stack from the master repo. This will allow us to test the entire stack together and ensure that everything is working correctly before deploying to staging or production.

We can encapsulate the entire flow into a single command such as `make deploy-dev` which will handle the deployment of all components to the dev environment.


### Staging environment
This environment will be used for testing the staging version of the code. We can use a similar command such as `make deploy-staging` to handle the deployment of all components to the staging environment

### Production environment
This environment will be used for deploying the production version of the code. We can use a similar command such as `make deploy-production` to handle the deployment of all components to the production environment.

Make deploy prod CANNOT be run by a single developer and will need to be run by 2/3 of the owners of the repo to ensure that the deployment is done correctly and that there are no issues with the codebase. This will help to ensure that the production environment is stable and that any issues are caught before they can cause problems for users.


### Monitoring and Post-deployment checks
After deployment, we will have monitoring and post-deployment checks in place to ensure that everything is working correctly. This will include monitoring the health of the different components, checking for any errors or issues, and ensuring that the system is performing as expected. We can use tools such as Prometheus and Grafana for monitoring.

We will have a command such as `post-deploy-check-dev` which will run a full set of integration tests against the dev environment to ensure that everything is working correctly after deployment. We can have similar commands for staging and production environments as well.


### Deployment workflow
1. Developers will work on their respective components in their own branches and repositories.
2. When a developer is ready to deploy, they will create a pull request to merge their changes into the dev branch of the master repo.
3. The pull request will be reviewed by other developers and owners of the repo to ensure that the changes are correct and do not introduce any issues.
4. Once the pull request is approved, the changes will be merged into the dev branch of the master repo.
5. The `make deploy-dev` command will be run to deploy the changes to the dev environment.
6. After deployment, the `post-deploy-check-dev` command will be run to ensure that everything is working correctly in the dev environment.
7. If everything is working correctly, the changes will be merged into the staging branch of the master repo and the `make deploy-staging` command will be run to deploy the changes to the staging environment.
8. After deployment, the `post-deploy-check-staging` command will be run to ensure that everything is working correctly in the staging environment.
9. If everything is working correctly, the changes will be merged into the production branch of the master repo and the `make deploy-production` command will be run to deploy the changes to the production environment. This step will require approval from 2/3 of the owners of the repo to ensure that the deployment is done correctly and that there are no issues with the codebase.
10. After deployment, the `post-deploy-check-production` command will be run to ensure that everything is working correctly in the production environment.

### Deployment rollback
In case of any issues during deployment, we will have a rollback mechanism in place. This will allow us to quickly revert to the previous stable version of the codebase in case of any issues or errors. We can use git tags to mark stable versions of the codebase and use those tags to quickly roll back to a previous version if needed. This will be done using a forwards looking approach whereby we merge a new branch with the previous stable version of the codebase and then deploy that branch to the respective environment. This will allow us to quickly revert to a previous version without having to manually revert changes in the codebase.

### Deployment tooling.

We will use ansible for the deployment and configuration of the different components. This means that for the `dev` and `staging` environments we can have a single ansible playbook that will handle the deployment of all components to the respective environment. For the `production` environment, we can have a separate ansible playbook that will handle the deployment of all components to the production environment. This will allow us to have more control over the deployment process for the production environment and ensure that everything is done correctly. It will also allow us to have different configurations for the different environments, and add all of the deployments to github actions for automation and ease of use.


## Conclusion
This proposal outlines a new structure for our codebase that will allow us to maintain the benefits of separation of concerns while also simplifying the management of the codebase. By using a master repo with submodules, we can effectively use the mono-repo approach while still maintaining the benefits of having separate repositories for each component. Additionally, by implementing a structured deployment workflow and post-deployment checks, we can ensure that our deployment process is efficient and effective while also maintaining the stability of our production environment. Overall, this approach can help to improve the efficiency and effectiveness of our development process while still maintaining the benefits of separation of concerns.

