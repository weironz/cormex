# Control node image for `just deploy` where ansible-playbook is not installed.
#
# Ansible's control node has to be POSIX: ansible-playbook does not run on Windows.
# This image is the answer for a Windows checkout -- Docker Desktop already has to be
# there for the deployment itself -- and it keeps the version pinned so the playbook
# is not at the mercy of whatever Python packaging the host happens to have.
FROM python:3.12-slim

RUN pip install --no-cache-dir ansible-core==2.21.3 && rm -rf /root/.cache/pip

ENV ANSIBLE_HOST_KEY_CHECKING=True
WORKDIR /repo
ENTRYPOINT ["ansible-playbook"]
