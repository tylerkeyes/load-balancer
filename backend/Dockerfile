# Use Python base image
FROM python:alpine

# Set the working directory
WORKDIR /app

# Copy static files to the container
COPY ./server8080/index.html /app

# Expose a default port (e.g., 8000)
EXPOSE 8000

# Use the environment variable PORT, with a default of 8000
ENV PORT=8000

# Run Python's HTTP server using the specified port
CMD ["sh", "-c", "python3 -m http.server ${PORT}"]
