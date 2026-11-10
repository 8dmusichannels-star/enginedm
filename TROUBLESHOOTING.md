## Troubleshooting

- **Black first greeter after boot:** raise `weston_delay` in `/etc/conf.d/enginedm`.
- **Virtual machine without GPU acceleration:** set `weston_args="--renderer=pixman"`.
- **No picture shows:** make sure the build includes the `wallpaper` feature.
