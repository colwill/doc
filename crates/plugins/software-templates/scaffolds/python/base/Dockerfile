FROM python:3.12-slim AS build
WORKDIR /src
COPY pyproject.toml ./
COPY src ./src
RUN pip install --no-cache-dir --target /install .

FROM python:3.12-slim
WORKDIR /app
RUN useradd --uid 10001 --create-home service
COPY --from=build /install /usr/local/lib/python3.12/site-packages
USER 10001
ENTRYPOINT ["python", "-m", "{{ scaffold.package }}"]
