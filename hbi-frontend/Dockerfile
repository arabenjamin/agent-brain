FROM node:22-alpine AS build
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
# A template: the key is filled in at start, never baked into the image.
COPY nginx.conf /etc/nginx/templates/default.conf.template
EXPOSE 80
