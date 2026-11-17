"""Shared PDF display-frame to PDF user-space mapping.

Poppler's pdftoppm/pdftotext use the MediaBox unless explicitly passed
-cropbox. OCR and native-word boxes must therefore map from the MediaBox,
not from the CropBox visible in many PDF viewers. Ratios cancel UserUnit;
rotation and nonzero page-box origins are handled by the affine transform.
"""


def inherited(page, key, default):
    item = page.obj
    visited = set()
    while item is not None:
        # PDF page-tree cycles are malformed; bounded iteration is preferable
        # to an infinite loop on untrusted input.
        identity = getattr(item, "objgen", None)
        if identity is not None and identity in visited:
            raise ValueError("PDF page hierarchy contains a cycle")
        if identity is not None:
            visited.add(identity)
        if key in item:
            return item[key]
        item = item.get("/Parent")
    return default


def display_mapper(page, page_box="mediabox"):
    # UserUnit already affects raster and Poppler native word pixel units,
    # so the inverse-display normalized-to-page ratios cancel it.
    if page_box not in ("cropbox", "mediabox"):
        raise ValueError("unknown PDF display page box")
    box = page.cropbox if page_box == "cropbox" else page.mediabox
    x0, y0, x1, y1 = (float(v) for v in box)
    rotation = int(inherited(page, "/Rotate", 0)) % 360
    if rotation not in (0, 90, 180, 270):
        raise ValueError(f"unsupported non-quarter-turn page rotation {rotation}")
    width, height = x1 - x0, y1 - y0
    if width <= 0 or height <= 0:
        raise ValueError(f"invalid page {page_box}")

    def transform(u, v):
        if rotation == 0:
            return [x0 + u * width, y1 - v * height]
        if rotation == 90:
            return [x0 + v * width, y0 + u * height]
        if rotation == 180:
            return [x1 - u * width, y0 + v * height]
        return [x1 - v * width, y1 - u * height]

    return transform, rotation


def inverse_display_mapper(page, page_box="mediabox"):
    """Map a PDF user-space point to normalized raster X/right, Y/down.

    The forward mapper handles the selected page box and inherited Rotate.
    UserUnit cancels in the normalized-to-user-space ratios. Invert its full affine basis rather than
    branching independently on page rotation and risking drift.
    """
    forward, rotation = display_mapper(page, page_box=page_box)
    origin = forward(0.0, 0.0)
    ux = forward(1.0, 0.0)
    vy = forward(0.0, 1.0)
    ax, ay = ux[0] - origin[0], ux[1] - origin[1]
    bx, by = vy[0] - origin[0], vy[1] - origin[1]
    determinant = ax * by - ay * bx
    if not abs(determinant) > 1e-12:
        raise ValueError("PDF display transform is not invertible")

    def transform(x, y):
        dx, dy = x - origin[0], y - origin[1]
        return ((dx * by - dy * bx) / determinant, (ax * dy - ay * dx) / determinant)

    return transform, rotation
